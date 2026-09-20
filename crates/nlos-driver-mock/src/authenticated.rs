//! ADR-0011 authenticated serving entry for the mock driver (Unix only).
//!
//! This module is the only transport entry this crate opens: the plain
//! trust-domain prefix is deliberately absent — a provider face is a
//! side-effecting authority boundary, so every request byte must arrive
//! behind the principal challenge-response handshake
//! ([ADR-0011](https://github.com/cty12356541/llmos/blob/main/docs/management/adrs/0011-ipc-principal-auth-signature-passthrough.md)).
//!
//! [`AuthenticatedMockDriverServer::serve_one`] composes the `nlos-ipc`
//! handshake facility in its fail-closed order:
//!
//! 1. `UnixListenerAdapter::accept` plus the caller's [`PeerAuthorizer`]
//!    pre-gate — before any handshake side effect;
//! 2. one challenge issued from the [`ServerHandshakeContext`]'s one-time
//!    nonce registry and bound to its locally derived channel binding;
//! 3. attestation verification through the [`IdentityAuthority`] with
//!    `verified_at_ms` taken from the [`AuthorityClock`] durable wall reading
//!    (`inspect_wall`, the side-effect-free verification read);
//! 4. the plain `serve_one` semantics, with the verified principal threaded
//!    into every authorization decision of the shared core.
//!
//! Every handshake failure is a typed [`HandshakeError`]; the connection is
//! dropped before any request byte is dispatched and a failed handshake
//! burns its nonce by design.

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use nlos_clock::{AuthorityClock, AuthorityClockError};
use nlos_identity::IdentityAuthority;
use nlos_ipc::handshake::transport::ServerHandshakeContext;
use nlos_ipc::handshake::{
    HandshakeError, VerifiedPrincipalHandshake, decode_attestation_wire, encode_challenge_wire,
    issue_challenge, verify_attestation,
};
use nlos_ipc::unix::UnixListenerAdapter;
use nlos_ipc::{FramedIo, IpcError, OutboundResponse, PeerAuthorizer, TransportConfig, serve_one};
use nlos_schema::HANDSHAKE_NONCE_BYTES;
use nlos_schema::sabi::v1::ExchangeResponse;
use nlos_types::PrincipalId;

use crate::ipc::MockDriverService;
use crate::provider::MockProvider;

/// Typed failures of one authenticated serve cycle. Handshake failures keep
/// the facility's exact typed surface; the wall reading adds one clock
/// variant so a caller can distinguish "the connection lied" from "the local
/// clock authority is unavailable".
#[derive(Debug)]
pub enum AuthenticatedMockDriverError {
    /// The challenge-response handshake or the OS-credential pre-gate
    /// rejected the connection before any request byte was served.
    Handshake(HandshakeError),
    /// The [`AuthorityClock`] could not produce its durable wall reading.
    Clock(AuthorityClockError),
}

impl fmt::Display for AuthenticatedMockDriverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Handshake(error) => {
                write!(
                    formatter,
                    "authenticated mock driver handshake failed: {error}"
                )
            }
            Self::Clock(error) => write!(
                formatter,
                "authenticated mock driver could not read the clock authority wall: {error}"
            ),
        }
    }
}

impl Error for AuthenticatedMockDriverError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Handshake(error) => Some(error),
            Self::Clock(error) => Some(error),
        }
    }
}

impl From<HandshakeError> for AuthenticatedMockDriverError {
    fn from(error: HandshakeError) -> Self {
        Self::Handshake(error)
    }
}

impl From<AuthorityClockError> for AuthenticatedMockDriverError {
    fn from(error: AuthorityClockError) -> Self {
        Self::Clock(error)
    }
}

/// Outcome of one [`AuthenticatedMockDriverServer::serve_one`] cycle: the
/// principal identity the handshake verified, and the post-handshake serve
/// result.
#[derive(Debug)]
pub struct AuthenticatedMockDriverOutcome {
    verified: VerifiedPrincipalHandshake,
    served: Result<(), IpcError>,
}

impl AuthenticatedMockDriverOutcome {
    #[must_use]
    pub const fn verified(&self) -> &VerifiedPrincipalHandshake {
        &self.verified
    }

    pub const fn served(&self) -> &Result<(), IpcError> {
        &self.served
    }

    pub fn into_parts(self) -> (VerifiedPrincipalHandshake, Result<(), IpcError>) {
        (self.verified, self.served)
    }
}

/// Opt-in authenticated binding of one mock driver endpoint: the shared
/// [`MockProvider`] core, the SABI policy authorizer, the attestation
/// verifier (`IdentityAuthority`), the verified-at source (`AuthorityClock`),
/// and the endpoint's [`ServerHandshakeContext`] (local channel binding plus
/// one-time nonce registry).
pub struct AuthenticatedMockDriverServer<A> {
    provider: Arc<MockProvider>,
    authorizer: A,
    identity: IdentityAuthority,
    clock: AuthorityClock,
    handshake: ServerHandshakeContext,
}

impl<A> AuthenticatedMockDriverServer<A> {
    #[must_use]
    pub const fn new(
        provider: Arc<MockProvider>,
        authorizer: A,
        identity: IdentityAuthority,
        clock: AuthorityClock,
        handshake: ServerHandshakeContext,
    ) -> Self {
        Self {
            provider,
            authorizer,
            identity,
            clock,
            handshake,
        }
    }
}

impl<A> AuthenticatedMockDriverServer<A>
where
    A: crate::ipc::MockDriverAuthorizer,
{
    /// Accepts exactly one connection and serves it under ADR-0011
    /// authentication, in the handshake facility's fail-closed order: OS
    /// pre-gate, challenge, attestation, then the one-request `serve_one`
    /// semantics with the verified principal threaded into every policy
    /// decision. `verified_at_ms` is the clock authority's durable wall
    /// reading; this server never takes a durable side effect on the read
    /// path.
    ///
    /// # Errors
    ///
    /// Fails closed with a typed handshake error for pre-gate denial,
    /// transport failures, schema violations, wrong or replayed nonces,
    /// channel-binding drift, unknown principals, revoked or invalid keys,
    /// and bad signatures — in every such case before any request byte is
    /// dispatched. A failed or unavailable wall reading is a typed clock
    /// error; the burned nonce stays consumed.
    pub async fn serve_one<P, N>(
        &self,
        listener: &UnixListenerAdapter,
        config: TransportConfig,
        peer_gate: &P,
        now_monotonic_ns: u64,
        next_nonce: N,
    ) -> Result<AuthenticatedMockDriverOutcome, AuthenticatedMockDriverError>
    where
        P: PeerAuthorizer,
        N: FnOnce() -> [u8; HANDSHAKE_NONCE_BYTES],
    {
        let (stream, peer) = listener
            .accept(config)
            .await
            .map_err(HandshakeError::Transport)?;
        peer_gate
            .authorize(&peer)
            .map_err(HandshakeError::PeerAuthorization)?;

        let mut framed = FramedIo::new(stream, config);
        let challenge = issue_challenge(self.handshake.nonces(), next_nonce())?;
        framed
            .send(&encode_challenge_wire(&challenge)?)
            .await
            .map_err(HandshakeError::Transport)?;
        let attestation_wire = framed.receive().await.map_err(HandshakeError::Transport)?;
        let attestation = decode_attestation_wire(&attestation_wire)?;

        let verified_at_ms = self.clock.inspect_wall()?.as_u64();
        let verified = verify_attestation(
            &self.identity,
            self.handshake.nonces(),
            &attestation,
            self.handshake.binding(),
            verified_at_ms,
        )?;

        let principal: PrincipalId = verified.principal_id();
        let service = MockDriverService::new(Arc::clone(&self.provider), &self.authorizer);
        let served = serve_one(
            framed.into_inner(),
            config,
            peer,
            peer_gate,
            move |request| {
                let response =
                    service.handle_for_ipc(request.envelope(), principal, now_monotonic_ns);
                async move {
                    Ok(OutboundResponse::Typed(ExchangeResponse {
                        envelope: Some(response),
                    }))
                }
            },
        )
        .await;
        Ok(AuthenticatedMockDriverOutcome { verified, served })
    }
}
