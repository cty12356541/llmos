//! Cluster deployment profiles and the `N = 1` single-host desktop zero-socket
//! guarantee (`ADR-0026` §3.1, §6.3, `[NLOS-GOAL-001]`).

use std::error::Error;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use nlos_cell::CellIdentity;

/// Deployment topology profile governing whether `nlos-net` may open network
/// sockets (`ADR-0026` §6.3).
///
/// The default is [`Self::SingleHostDesktop`] (`N = 1`): NLOS binds **zero**
/// TCP/UDP ports, triggers zero OS firewall prompts, requires zero TLS
/// certificates, and keeps all local authority communication on `nlos-ipc`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ClusterProfile {
    /// Default single-PC desktop deployment (`N = 1`): zero network listeners
    /// and zero outbound cross-host sockets permitted.
    #[default]
    SingleHostDesktop,
    /// Explicit multi-host cluster deployment (`N >= 2` or explicit single-node
    /// network seed): enables TLS 1.3 mTLS listener/connector bound to
    /// `local_cell`.
    MultiHostCluster {
        /// Opaque 16-byte nominal identity of the local Cell (`[DIST-NAME-001]`).
        local_cell: CellIdentity,
        /// Ephemeral local socket bind hint (never part of `CellIdentity`).
        bind_hint: SocketAddr,
    },
}

impl ClusterProfile {
    /// Returns `true` when running in the default `N = 1` single-host desktop
    /// zero-network profile.
    #[must_use]
    pub const fn is_single_host_desktop(&self) -> bool {
        matches!(self, Self::SingleHostDesktop)
    }

    /// Returns `true` when cross-host network sockets are explicitly enabled.
    #[must_use]
    pub const fn allows_network_sockets(&self) -> bool {
        matches!(self, Self::MultiHostCluster { .. })
    }

    /// Enforces that network socket binding is permitted under this profile.
    ///
    /// # Errors
    ///
    /// Fails closed with [`SingleHostProfileViolation`] when `self` is
    /// [`Self::SingleHostDesktop`].
    pub const fn require_multi_host_socket_permission(
        &self,
        attempted_op: &'static str,
    ) -> Result<(CellIdentity, SocketAddr), SingleHostProfileViolation> {
        match *self {
            Self::SingleHostDesktop => Err(SingleHostProfileViolation { attempted_op }),
            Self::MultiHostCluster {
                local_cell,
                bind_hint,
            } => Ok((local_cell, bind_hint)),
        }
    }
}

/// Fail-closed refusal when attempting to bind or dial a network socket under
/// [`ClusterProfile::SingleHostDesktop`] (`ADR-0026` §6.3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SingleHostProfileViolation {
    /// Static name of the forbidden network operation.
    pub attempted_op: &'static str,
}

impl fmt::Display for SingleHostProfileViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "ClusterProfile::SingleHostDesktop (N=1) forbids network socket operation '{}'",
            self.attempted_op
        )
    }
}

impl Error for SingleHostProfileViolation {}

/// Observable socket accounting ledger proving zero-socket overhead in `N = 1`
/// mode and tracking active listeners/connections in `MultiHostCluster` mode.
#[derive(Clone, Debug, Default)]
pub struct SocketActivityAccounting {
    inner: Arc<AccountingInner>,
}

#[derive(Debug, Default)]
struct AccountingInner {
    bound_listeners: AtomicUsize,
    peak_bound_listeners: AtomicUsize,
    active_connections: AtomicUsize,
    peak_active_connections: AtomicUsize,
}

impl SocketActivityAccounting {
    /// Creates a fresh zeroed socket accounting ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Current count of bound TCP listeners owned by this runtime.
    #[must_use]
    pub fn bound_listener_count(&self) -> usize {
        self.inner.bound_listeners.load(Ordering::Acquire)
    }

    /// Peak count of bound TCP listeners ever simultaneously open.
    #[must_use]
    pub fn peak_bound_listener_count(&self) -> usize {
        self.inner.peak_bound_listeners.load(Ordering::Acquire)
    }

    /// Current count of active TLS/TCP peer connections owned by this runtime.
    #[must_use]
    pub fn active_connection_count(&self) -> usize {
        self.inner.active_connections.load(Ordering::Acquire)
    }

    /// Peak count of active TLS/TCP peer connections ever simultaneously open.
    #[must_use]
    pub fn peak_active_connection_count(&self) -> usize {
        self.inner.peak_active_connections.load(Ordering::Acquire)
    }

    pub(crate) fn acquire_listener_guard(&self) -> ListenerSocketGuard {
        let current = self.inner.bound_listeners.fetch_add(1, Ordering::AcqRel) + 1;
        self.inner
            .peak_bound_listeners
            .fetch_max(current, Ordering::AcqRel);
        ListenerSocketGuard {
            accounting: self.clone(),
        }
    }

    pub(crate) fn acquire_connection_guard(&self) -> ConnectionSocketGuard {
        let current = self.inner.active_connections.fetch_add(1, Ordering::AcqRel) + 1;
        self.inner
            .peak_active_connections
            .fetch_max(current, Ordering::AcqRel);
        ConnectionSocketGuard {
            accounting: self.clone(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ListenerSocketGuard {
    accounting: SocketActivityAccounting,
}

impl Drop for ListenerSocketGuard {
    fn drop(&mut self) {
        self.accounting
            .inner
            .bound_listeners
            .fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
pub(crate) struct ConnectionSocketGuard {
    accounting: SocketActivityAccounting,
}

impl Drop for ConnectionSocketGuard {
    fn drop(&mut self) {
        self.accounting
            .inner
            .active_connections
            .fetch_sub(1, Ordering::AcqRel);
    }
}
