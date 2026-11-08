//! Daemon-mounted `WaitControl` adapter (`L-009`, W82-A): the frozen v1.0
//! `service = "wait_control"` channel behind the system-control daemon's
//! five-way dispatch, powered by the shared durable [`WaitAuthority`] the
//! daemon already owns (`SystemControlDaemon::waits`).
//!
//! Two faces over one authority:
//!
//! - [`DaemonWaitControl::plain`] — the local trust-domain face for the
//!   daemon's plaintext endpoint: the unmodified
//!   [`WaitControlService`](nlos_wait_control::WaitControlService) with an
//!   allow-all authorizer, mirroring the daemon's existing posture for the
//!   `channel` service (`AllowAllChannelControlAuthorizer`): the endpoints
//!   stay inside the local trust domain (owner-only sockets / the
//!   machine-local pipe namespace), so the capability model remains the
//!   registered boundary (ADR-0011 decision 2), same as every sibling
//!   service this daemon mounts.
//! - [`DaemonWaitControl::authenticated`] — the ADR-0011 face for the
//!   handshake-verified endpoint: the sibling crate's
//!   [`AuthenticatedWaitControl`], which binds every mutation idempotency key
//!   to the connection's verified principal before the plain service sees
//!   the request (same principal + same key ⇒ same durable replay identity;
//!   different principals never collide on attacker-chosen key bytes).
//!   Queries pass through byte-identical. This is exactly the pattern the
//!   `nlos-wait-control` authenticated variant demonstrates for its own
//!   endpoint, reused here instead of re-derived.

use std::sync::Arc;

use nlos_ipc::handshake::VerifiedPrincipalHandshake;
use nlos_schema::sabi::v1::Envelope;
use nlos_wait::WaitAuthority;
use nlos_wait_control::authenticated::AuthenticatedWaitControl;
use nlos_wait_control::payload;
use nlos_wait_control::{WaitControlAuthorizer, WaitControlService};

/// Re-export of the frozen v1.0 service name (`"wait_control"`), so the
/// dispatch table and callers share one constant with the sibling crate.
pub use nlos_wait_control::WAIT_CONTROL_SERVICE;

/// Local trust-domain authorizer for the daemon-mounted `WaitControl` face:
/// admits every request, mirroring [`AllowAllChannelControlAuthorizer`]. The
/// capability/principal model is the registered ADR-0011 decision 2 boundary;
/// this seam exists so a future policy lands in one place.
pub struct AllowAllWaitControlAuthorizer;

impl WaitControlAuthorizer for AllowAllWaitControlAuthorizer {
    fn authorize_register_wait(
        &self,
        _: &nlos_schema::sabi::v1::SabiRequestContext,
        _: &payload::RegisterWaitRequest,
    ) -> Result<(), &'static str> {
        Ok(())
    }

    fn authorize_notify_commits(
        &self,
        _: &nlos_schema::sabi::v1::SabiRequestContext,
        _: &payload::NotifyCommitsRequest,
    ) -> Result<(), &'static str> {
        Ok(())
    }

    fn authorize_cancel_wait(
        &self,
        _: &nlos_schema::sabi::v1::SabiRequestContext,
        _: &payload::CancelWaitRequest,
    ) -> Result<(), &'static str> {
        Ok(())
    }

    fn authorize_list_waits(
        &self,
        _: &nlos_schema::sabi::v1::SabiRequestContext,
        _: &payload::ListWaitsRequest,
    ) -> Result<(), &'static str> {
        Ok(())
    }

    fn authorize_inspect_wait(
        &self,
        _: &nlos_schema::sabi::v1::SabiRequestContext,
        _: &payload::InspectWaitRequest,
    ) -> Result<(), &'static str> {
        Ok(())
    }
}

/// The static allow-all policy the authenticated face borrows
/// ([`AuthenticatedWaitControl`] takes the authorizer by reference).
const ALLOW_ALL_WAIT: AllowAllWaitControlAuthorizer = AllowAllWaitControlAuthorizer;

/// One daemon round's `WaitControl` face: plain (local trust domain) or
/// principal-bound (ADR-0011 authenticated connection). Constructed per
/// exchange like every other service in the daemon's handler round, so no
/// per-connection state can go stale.
pub enum DaemonWaitControl<'a> {
    Plain(WaitControlService<AllowAllWaitControlAuthorizer>),
    Authenticated(AuthenticatedWaitControl<'a, AllowAllWaitControlAuthorizer>),
}

impl DaemonWaitControl<'_> {
    /// Local trust-domain face over the daemon's durable wait authority.
    #[must_use]
    pub fn plain(waits: &Arc<WaitAuthority>) -> Self {
        Self::Plain(WaitControlService::new(
            Arc::clone(waits),
            AllowAllWaitControlAuthorizer,
        ))
    }

    /// Principal-bound face for one ADR-0011-authenticated connection: every
    /// mutation idempotency key is bound to the handshake-verified principal
    /// before the plain service validates the request.
    #[must_use]
    pub fn authenticated(waits: &Arc<WaitAuthority>, verified: VerifiedPrincipalHandshake) -> Self {
        Self::Authenticated(AuthenticatedWaitControl::new(
            waits,
            &ALLOW_ALL_WAIT,
            verified,
        ))
    }

    /// Handles one `WaitControl` request and always returns a typed response
    /// envelope (handler failures become bounded failure envelopes; the five
    /// frozen methods are `register_wait` / `notify_commits` / `cancel_wait` /
    /// `list_waits` / `inspect_wait`).
    #[must_use]
    pub fn handle_for_ipc(
        &self,
        request: &Envelope,
        now_monotonic_ns: u64,
        now_wall_ms: i64,
    ) -> Envelope {
        match self {
            Self::Plain(service) => service.handle_for_ipc(request, now_monotonic_ns, now_wall_ms),
            Self::Authenticated(service) => {
                service.handle_for_ipc(request, now_monotonic_ns, now_wall_ms)
            }
        }
    }
}
