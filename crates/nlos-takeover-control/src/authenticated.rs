//! ADR-0011 opt-in authenticated IPC serving for `TakeoverControl`.
//!
//! The default build and the feature-gated `takeover-control-conformance`
//! server keep serving connections without a principal-level handshake (OS
//! credential gating only). This module is the additive, opt-in variant:
//! built behind the `authenticated-ipc` feature, it composes the existing
//! transport-neutral handler with the ADR-0011 transport facility
//! `nlos-ipc::handshake::transport` — challenge-response handshake, one-time
//! nonce registry, endpoint channel binding — and binds the verification
//! instant to the [`AuthorityClock`] wall domain.
//!
//! # Two orthogonal signature layers
//!
//! `TakeoverControl` submissions are Ed25519-signed, and this module stacks a
//! second, transport-level signature on top. The two layers are independent
//! and must not be confused:
//!
//! * **Transport layer (this module).** The *connecting IPC principal*
//!   proves control of its current `SemanticSigning` key by signing the
//!   domain-separated `(nonce, principal id, channel binding)` digest. The
//!   proof is verified through the [`IdentityAuthority`] against the
//!   principal's *current* key binding, judged at the **`AuthorityClock`'s
//!   durable wall high-water** (see [`AuthorityClock::inspect_wall`]) —
//!   monotone across restarts and system-clock rollbacks, never guessed. The
//!   read is deliberately side-effect free: an unauthenticated connection
//!   can never drive a durable clock advance by connecting (denying the
//!   wall watermark to unauthenticated peers is a property, not an
//!   omission). A fresh clock store reads a high-water of 0, which fails
//!   any key with a positive `valid_from` — fail-closed; operators advance
//!   the wall domain through their normal duty cycle before serving.
//! * **Payload layer (unchanged).** The barrier observation carries its own
//!   Ed25519 signature over the fence-bound observation digest, made by the
//!   *remote participant's* `BarrierObservationSigning` key and verified by
//!   the `TaskAuthority` store path inside [`TakeoverControl::handle`]. The
//!   observation's durable time semantics (`observed_at_ms`, store-anchored
//!   verification) are byte-for-byte unchanged by this module; the
//!   transport-layer verification time is not recorded in the observation.
//!
//! The layers differ in principal (IPC caller vs remote observer), key
//! purpose (`SemanticSigning` vs `BarrierObservationSigning`), message
//! (handshake digest vs observation digest), and time anchoring (clock wall
//! high-water at handshake vs existing store semantics). Neither substitutes
//! for the other: authenticating the caller does not prove the observation,
//! and a valid observation signature does not authenticate the connection.
//!
//! # Fail-closed behavior
//!
//! Every handshake failure is a typed error and the connection is dropped
//! before any request byte reaches the handler; a consumed nonce is never
//! returned, so failed handshakes burn their nonce by design. Unix only:
//! the upstream transport facility is `#[cfg(unix)]`, so Windows
//! authenticated serving remains a future slice.

use std::error::Error;
use std::fmt;

use nlos_clock::{AuthorityClock, AuthorityClockError};
use nlos_identity::IdentityAuthority;
use nlos_ipc::handshake::HandshakeError;
use nlos_ipc::handshake::transport::{
    AuthenticatedServeOutcome, ServerHandshakeContext, authenticated_serve_one,
};
use nlos_ipc::unix::UnixListenerAdapter;
use nlos_ipc::{IpcError, OutboundResponse, PeerAuthorizer, TransportConfig};
use nlos_schema::sabi::v1::ExchangeResponse;
use nlos_schema::{HANDSHAKE_NONCE_BYTES, ValidatedExchangeRequest};
use nlos_task::SqliteTaskAuthority;

use crate::{TakeoverControl, TakeoverControlAuthorizer};

/// Typed failure of one authenticated serving cycle: the wall reading
/// refused (fail-closed, zero durable state) or the handshake rejected the
/// connection before any request was served.
#[derive(Debug)]
pub enum AuthenticatedIpcError {
    /// The [`AuthorityClock`] could not serve a wall reading; no time was
    /// guessed and the connection was not accepted into the handshake.
    Clock(AuthorityClockError),
    /// The transport-layer handshake failed closed; no request byte reached
    /// the handler.
    Handshake(HandshakeError),
}

impl fmt::Display for AuthenticatedIpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Clock(error) => write!(formatter, "authenticated serve clock failure: {error}"),
            Self::Handshake(error) => {
                write!(formatter, "authenticated serve handshake failure: {error}")
            }
        }
    }
}

impl Error for AuthenticatedIpcError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Clock(error) => Some(error),
            Self::Handshake(error) => Some(error),
        }
    }
}

impl From<AuthorityClockError> for AuthenticatedIpcError {
    fn from(error: AuthorityClockError) -> Self {
        Self::Clock(error)
    }
}

impl From<HandshakeError> for AuthenticatedIpcError {
    fn from(error: HandshakeError) -> Self {
        Self::Handshake(error)
    }
}

/// Opt-in authenticated serving variant of the [`TakeoverControl`] IPC
/// surface: the existing handler composed with the ADR-0011 transport
/// handshake and the [`AuthorityClock`] wall domain. See the module
/// documentation for the two-signature-layer model.
pub struct AuthenticatedTakeoverControl<'a, A> {
    control: TakeoverControl<'a, A>,
    identity: &'a IdentityAuthority,
    clock: &'a AuthorityClock,
    handshake: &'a ServerHandshakeContext,
}

impl<A> fmt::Debug for AuthenticatedTakeoverControl<'_, A> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthenticatedTakeoverControl")
            .finish_non_exhaustive()
    }
}

impl<'a, A> AuthenticatedTakeoverControl<'a, A>
where
    A: TakeoverControlAuthorizer,
{
    /// Binds the task authority, identity authority, capability authorizer,
    /// the wall-domain clock, and the endpoint's handshake context (channel
    /// binding plus nonce registry) into one authenticated serving gate. The
    /// listener passed to [`Self::serve_one`] must be bound to the same
    /// endpoint the handshake context was derived from.
    #[must_use]
    pub const fn new(
        tasks: &'a SqliteTaskAuthority,
        identity: &'a IdentityAuthority,
        authorizer: &'a A,
        clock: &'a AuthorityClock,
        handshake: &'a ServerHandshakeContext,
    ) -> Self {
        Self {
            control: TakeoverControl::new(tasks, identity, authorizer),
            identity,
            clock,
            handshake,
        }
    }

    /// Accepts one connection, runs the OS-credential pre-gate, verifies the
    /// challenge-response handshake at the **`AuthorityClock`'s durable wall
    /// high-water** (`inspect_wall`, read without durable side effects), and
    /// only then serves exactly one exchange with the unchanged
    /// [`TakeoverControl::handle_for_ipc`] semantics.
    ///
    /// The wall high-water may lag the system clock between duty-cycle
    /// advances; that is the documented behavior of the durable wall domain,
    /// and verification never invents a fresher time. The handshake nonce is
    /// consumed by verification even when the signature then fails, so
    /// replays and retry oracles burn their nonce by design.
    ///
    /// # Errors
    ///
    /// Returns [`AuthenticatedIpcError::Clock`] when the wall reading fails
    /// closed, or [`AuthenticatedIpcError::Handshake`] for every typed
    /// pre-gate, transport, schema, nonce, binding, identity, and signature
    /// rejection. Post-handshake handler outcomes (including typed failure
    /// envelopes) are reported in [`AuthenticatedServeOutcome::served`], not
    /// as handshake errors.
    pub async fn serve_one<P, N>(
        &self,
        listener: &UnixListenerAdapter,
        config: TransportConfig,
        peer_gate: &P,
        now_monotonic_ns: u64,
        now_wall_ms: i64,
        next_nonce: N,
    ) -> Result<AuthenticatedServeOutcome, AuthenticatedIpcError>
    where
        P: PeerAuthorizer,
        N: FnMut() -> [u8; HANDSHAKE_NONCE_BYTES],
    {
        // Transport-layer verification instant: the durable wall high-water,
        // read with zero durable side effects (see module docs).
        let verified_at_ms = self.clock.inspect_wall()?.as_u64();
        let control = &self.control;
        let handler = move |validated: ValidatedExchangeRequest| {
            let envelope =
                control.handle_for_ipc(validated.envelope(), now_monotonic_ns, now_wall_ms);
            async move {
                Ok::<_, IpcError>(OutboundResponse::Typed(ExchangeResponse {
                    envelope: Some(envelope),
                }))
            }
        };
        Ok(authenticated_serve_one(
            listener,
            config,
            self.identity,
            self.handshake.nonces(),
            self.handshake.binding(),
            peer_gate,
            handler,
            next_nonce,
            verified_at_ms,
        )
        .await?)
    }
}
