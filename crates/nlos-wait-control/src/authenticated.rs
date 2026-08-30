//! ADR-0011 authenticated `WaitControl` service variant (opt-in).
//!
//! This module layers the ADR-0011 principal challenge-response handshake in
//! front of the plain [`WaitControlService`] without touching it: the local
//! trust-domain path, its authorization posture, and the conformance server
//! keep their exact behavior and byte surface. Nothing here is on any default
//! feature path; the whole module requires the `authenticated-server` feature
//! and a Unix host.
//!
//! # Composition (one connection)
//!
//! [`AuthenticatedWaitControlServer::serve_one`] composes the `nlos-ipc`
//! handshake transport facility in the facility's fail-closed order:
//!
//! 1. `UnixListenerAdapter::accept` plus the caller's [`PeerAuthorizer`]
//!    pre-gate — before any handshake side effect;
//! 2. one challenge issued from the [`ServerHandshakeContext`]'s one-time
//!    nonce registry and bound to its locally derived channel binding;
//! 3. attestation verification through the [`IdentityAuthority`] with
//!    `verified_at_ms` taken from the [`AuthorityClock`] durable wall reading
//!    ([`AuthorityClock::inspect_wall`], the side-effect-free verification
//!    read — rollback-absorbed, never guessed);
//! 4. the plain `serve_one` semantics, with the verified principal threaded
//!    into the handler so the request path can bind idempotency keys.
//!
//! Every handshake failure is a typed [`HandshakeError`]; the connection is
//! dropped before any request byte is dispatched and the consumed nonce is
//! never returned to the registry, so a failed handshake burns its nonce by
//! design.
//!
//! # Principal-bound mutation idempotency (the ADR-0011 unlock)
//!
//! The shared [`WaitAuthority`] replays durable receipts under the raw
//! idempotency key bytes. Across principals those bytes are attacker-chosen
//! and unregistered, so two principals reusing the same key could collide or
//! replay each other's mutations in the durable registry. On this
//! authenticated path every mutation key is therefore namespaced by the
//! verified principal before the plain service sees the request
//! ([`principal_bound_idempotency_key`]): deterministic (same principal plus
//! same key ⇒ same durable replay identity), domain-separated from every
//! other digest in the system, and collision-free across principals. Query
//! methods carry no idempotency semantics and pass through byte-identical.
//!
//! Receipts that reference the request key (notify / cancellation) report the
//! bound key bytes on this path; a registration's receipt stays the durable
//! `WaitId`. A host that must map a bound key back to its origin combines the
//! verified principal (from the connection's [`AuthenticatedWaitOutcome`])
//! with the original request key.

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
use nlos_schema::sabi::v1::{Envelope, ExchangeResponse, SabiRequestContext, envelope};
use nlos_types::PrincipalId;
use nlos_wait::WaitAuthority;
use sha2::{Digest, Sha256};

use crate::{
    CANCEL_WAIT_METHOD, NOTIFY_COMMITS_METHOD, REGISTER_WAIT_METHOD, WaitControlAuthorizer,
    WaitControlError, WaitControlService, decode_cancel_wait_request,
    decode_notify_commits_request, decode_register_wait_request, encode_cancel_wait_request,
    encode_notify_commits_request, encode_register_wait_request, failure_envelope,
};

/// Domain separator keeping principal-bound mutation idempotency keys
/// distinct from every other SHA-256 domain in the system.
const AUTHENTICATED_IDEMPOTENCY_DOMAIN: &[u8] = b"llmos/wait-control/auth-idem/v1";

/// Derives the durable mutation idempotency key used on the authenticated
/// path: the domain-separated SHA-256 of `(domain, principal id, original
/// key)`, truncated to the 16-byte `IdempotencyKey` width. The first half of
/// the digest is the bound key; truncation keeps the authority's key width
/// contract while 128 bits remain far beyond brute-force reach for keys that
/// must already be known to the caller.
///
/// The derivation is total and deterministic: a principal replaying its own
/// request replays the same durable receipt, while the same key bytes under a
/// different principal derive a different key.
#[must_use]
pub fn principal_bound_idempotency_key(principal: PrincipalId, key: [u8; 16]) -> [u8; 16] {
    let digest = Sha256::new()
        .chain_update(AUTHENTICATED_IDEMPOTENCY_DOMAIN)
        .chain_update(principal.as_bytes())
        .chain_update(key)
        .finalize();
    let mut bound = [0_u8; 16];
    bound.copy_from_slice(&digest[..16]);
    bound
}

/// Typed failures of one authenticated serve cycle. Handshake failures keep
/// the facility's exact typed surface; the wall reading adds one clock
/// variant so a caller can distinguish "the connection lied" from "the local
/// clock authority is unavailable".
#[derive(Debug)]
pub enum AuthenticatedWaitControlError {
    /// The challenge-response handshake or the OS-credential pre-gate
    /// rejected the connection before any request byte was served.
    Handshake(HandshakeError),
    /// The [`AuthorityClock`] could not produce its durable wall reading.
    Clock(AuthorityClockError),
}

impl fmt::Display for AuthenticatedWaitControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Handshake(error) => {
                write!(
                    formatter,
                    "authenticated WaitControl handshake failed: {error}"
                )
            }
            Self::Clock(error) => write!(
                formatter,
                "authenticated WaitControl could not read the clock authority wall: {error}"
            ),
        }
    }
}

impl Error for AuthenticatedWaitControlError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Handshake(error) => Some(error),
            Self::Clock(error) => Some(error),
        }
    }
}

impl From<HandshakeError> for AuthenticatedWaitControlError {
    fn from(error: HandshakeError) -> Self {
        Self::Handshake(error)
    }
}

impl From<AuthorityClockError> for AuthenticatedWaitControlError {
    fn from(error: AuthorityClockError) -> Self {
        Self::Clock(error)
    }
}

/// Outcome of one [`AuthenticatedWaitControlServer::serve_one`] cycle: the
/// principal identity the handshake verified, and the post-handshake serve
/// result.
#[derive(Debug)]
pub struct AuthenticatedWaitOutcome {
    verified: VerifiedPrincipalHandshake,
    served: Result<(), IpcError>,
}

impl AuthenticatedWaitOutcome {
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

/// The five `WaitControl` methods served under one verified principal: the
/// plain [`WaitControlService`] with every mutation idempotency key bound to
/// the principal before validation. Validation, authorization, and the
/// failure-envelope mapping run unchanged inside the plain service, so the
/// authenticated path cannot drift from the local trust-domain semantics.
pub struct AuthenticatedWaitControl<'a, A> {
    inner: WaitControlService<&'a A>,
    principal: PrincipalId,
}

impl<'a, A> AuthenticatedWaitControl<'a, A>
where
    A: WaitControlAuthorizer,
{
    #[must_use]
    pub fn new(
        waits: &Arc<WaitAuthority>,
        authorizer: &'a A,
        verified: VerifiedPrincipalHandshake,
    ) -> Self {
        Self {
            inner: WaitControlService::new(Arc::clone(waits), authorizer),
            principal: verified.principal_id(),
        }
    }

    #[must_use]
    pub const fn principal(&self) -> PrincipalId {
        self.principal
    }

    /// Handles one request with principal-bound mutation idempotency keys.
    /// Well-formed mutation requests carry the derived bound key in both the
    /// SABI context and the payload, so the plain service's key-equality
    /// binding holds and the authority replays under the bound key. Anything
    /// not a well-formed mutation passes through untouched and surfaces the
    /// plain path's exact typed failure.
    ///
    /// # Errors
    ///
    /// Returns the plain service's typed errors unchanged.
    pub fn handle(
        &self,
        request: &Envelope,
        now_monotonic_ns: u64,
        now_wall_ms: i64,
    ) -> Result<Envelope, WaitControlError> {
        let bound = bind_request_to_principal(request, self.principal);
        self.inner.handle(&bound, now_monotonic_ns, now_wall_ms)
    }

    /// [`Self::handle`] for a local IPC adapter: typed failures become the
    /// bounded failure envelope instead.
    #[must_use]
    pub fn handle_for_ipc(
        &self,
        request: &Envelope,
        now_monotonic_ns: u64,
        now_wall_ms: i64,
    ) -> Envelope {
        match self.handle(request, now_monotonic_ns, now_wall_ms) {
            Ok(response) => response,
            Err(error) => failure_envelope(request, &error),
        }
    }
}

/// Rewrites a well-formed mutation request onto its principal-bound keys.
/// A request that is not a mutation, or whose context/payload keys are
/// absent, unequal, or outside the 16-byte contract, is returned unchanged:
/// the plain service then reports the same typed failure it would have
/// reported on the local trust-domain path, so the authenticated surface
/// never invents a second rejection vocabulary.
fn bind_request_to_principal(request: &Envelope, principal: PrincipalId) -> Envelope {
    let rewritten = match request.method.as_str() {
        REGISTER_WAIT_METHOD => rewrite_register(request, principal),
        NOTIFY_COMMITS_METHOD => rewrite_notify(request, principal),
        CANCEL_WAIT_METHOD => rewrite_cancel(request, principal),
        _ => None,
    };
    let Some((payload_bytes, original_key)) = rewritten else {
        return request.clone();
    };
    let bound_key = principal_bound_idempotency_key(principal, original_key);
    let mut bound = request.clone();
    bound.payload = payload_bytes;
    if let Some(envelope::CommonContext::RequestContext(context)) = bound.common_context.as_mut() {
        context.idempotency_key = bound_key.to_vec();
    }
    bound
}

fn rewrite_register(request: &Envelope, principal: PrincipalId) -> Option<(Vec<u8>, [u8; 16])> {
    let context = request_context(request)?;
    let mut decoded = decode_register_wait_request(&request.payload).ok()?;
    let original = equal_contract_key(&decoded.idempotency_key, &context.idempotency_key)?;
    decoded.idempotency_key = principal_bound_idempotency_key(principal, original).to_vec();
    Some((encode_register_wait_request(&decoded).ok()?, original))
}

fn rewrite_notify(request: &Envelope, principal: PrincipalId) -> Option<(Vec<u8>, [u8; 16])> {
    let context = request_context(request)?;
    let mut decoded = decode_notify_commits_request(&request.payload).ok()?;
    let original = equal_contract_key(&decoded.idempotency_key, &context.idempotency_key)?;
    decoded.idempotency_key = principal_bound_idempotency_key(principal, original).to_vec();
    Some((encode_notify_commits_request(&decoded).ok()?, original))
}

fn rewrite_cancel(request: &Envelope, principal: PrincipalId) -> Option<(Vec<u8>, [u8; 16])> {
    let context = request_context(request)?;
    let mut decoded = decode_cancel_wait_request(&request.payload).ok()?;
    let original = equal_contract_key(&decoded.idempotency_key, &context.idempotency_key)?;
    decoded.idempotency_key = principal_bound_idempotency_key(principal, original).to_vec();
    Some((encode_cancel_wait_request(&decoded).ok()?, original))
}

fn request_context(request: &Envelope) -> Option<&SabiRequestContext> {
    match request.common_context.as_ref() {
        Some(envelope::CommonContext::RequestContext(context)) => Some(context),
        _ => None,
    }
}

/// The payload key iff it equals the context key and both satisfy the
/// 16-byte contract (the plain service's mutation key binding, evaluated
/// before any rewrite).
fn equal_contract_key(payload_key: &[u8], context_key: &[u8]) -> Option<[u8; 16]> {
    if payload_key != context_key {
        return None;
    }
    payload_key.try_into().ok()
}

/// Opt-in authenticated binding of one `WaitControl` endpoint: the shared
/// durable [`WaitAuthority`], the SABI policy authorizer, the attestation
/// verifier (`IdentityAuthority`), the verified-at source
/// (`AuthorityClock`), and the endpoint's [`ServerHandshakeContext`] (local
/// channel binding plus one-time nonce registry). Every field is held by
/// value; the server is shared by reference across connections.
pub struct AuthenticatedWaitControlServer<A> {
    waits: Arc<WaitAuthority>,
    authorizer: A,
    identity: IdentityAuthority,
    clock: AuthorityClock,
    handshake: ServerHandshakeContext,
}

impl<A> AuthenticatedWaitControlServer<A> {
    #[must_use]
    pub fn new(
        waits: Arc<WaitAuthority>,
        authorizer: A,
        identity: IdentityAuthority,
        clock: AuthorityClock,
        handshake: ServerHandshakeContext,
    ) -> Self {
        Self {
            waits,
            authorizer,
            identity,
            clock,
            handshake,
        }
    }
}

impl<A> AuthenticatedWaitControlServer<A>
where
    A: WaitControlAuthorizer,
{
    /// Accepts exactly one connection and serves it under ADR-0011
    /// authentication, in the handshake facility's fail-closed order: OS
    /// pre-gate, challenge, attestation, then the plain one-request
    /// [`serve_one`] semantics with the verified principal bound into the
    /// request path. `verified_at_ms` is the clock authority's durable wall
    /// reading; a host that wants real-time key-expiry checks advances the
    /// wall domain (`AuthorityClock::wall_now`) during its own bootstrap —
    /// this server never takes a durable side effect on the read path.
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
    ) -> Result<AuthenticatedWaitOutcome, AuthenticatedWaitControlError>
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
        let now_wall_ms = i64::try_from(verified_at_ms).map_err(|_| {
            AuthenticatedWaitControlError::Clock(AuthorityClockError::CorruptRecord(
                "wall reading exceeds i64",
            ))
        })?;
        let verified = verify_attestation(
            &self.identity,
            self.handshake.nonces(),
            &attestation,
            self.handshake.binding(),
            verified_at_ms,
        )?;

        let service = AuthenticatedWaitControl::new(&self.waits, &self.authorizer, verified);
        let served = serve_one(
            framed.into_inner(),
            config,
            peer,
            peer_gate,
            move |request| {
                let response =
                    service.handle_for_ipc(request.envelope(), now_monotonic_ns, now_wall_ms);
                async move {
                    Ok(OutboundResponse::Typed(ExchangeResponse {
                        envelope: Some(response),
                    }))
                }
            },
        )
        .await;
        Ok(AuthenticatedWaitOutcome { verified, served })
    }
}
