//! ADR-0011 opt-in authenticated control-plane entry points (Unix only).
//!
//! Strictly additive over [`crate::control`]: the in-process dispatcher, the
//! plain [`dispatch_over_socket`](crate::control::dispatch_over_socket)
//! route, and every conformance client that depends on the local trust-domain
//! path keep their exact semantics. This module only adds one explicit
//! opt-in service entry, [`authenticated_serve_one_control`], and its
//! matching client, [`dispatch_over_authenticated_socket`].
//!
//! Connection level (ADR-0011 decision 1): every connection must answer the
//! `nlos-ipc` principal challenge-response handshake. The server verifies
//! the attestation through the [`IdentityAuthority`] at the
//! **`AuthorityClock`'s durable wall reading** (ADR-0011 decision 3) instead
//! of caller-supplied wall time, and any handshake failure refuses the
//! connection before any request byte is dispatched.
//!
//! Command-level time semantics (chosen option: **request correlation**):
//! the served exchange's wall time is the `AuthorityClock`'s durable wall
//! reading issued (or durably replayed) for an idempotency key derived from
//! the request's §25.3 correlation id ([`command_wall_key`]). Mutations bind
//! that correlation id to their idempotency key by construction (the
//! handler's `CommandIdempotencyMismatch` guard), so a retried command
//! re-reads its original durable reading and its receipt timestamp never
//! drifts across retries. The correlation id must be exactly
//! [`REQUEST_ID_BYTES`] bytes; any other shape is a typed
//! `INVALID_ARGUMENT` failure envelope, never a guessed time.
//!
//! Handshake verified-at keys use a dedicated derivation from the one-time
//! server nonce (`llmos/control-auth/handshake-wall/v1`), so every
//! connection observes a fresh, monotone wall reading and a replayed
//! handshake can never re-read a stale instant.
//!
//! Known boundary: the handshake authenticates the *connection*. Binding
//! each command's issuer identity to the verified principal is ADR-0011
//! decision 2 (command-level signature passthrough) and stays with that
//! implementation slice.

use std::path::Path;

use nlos_clock::{AuthorityClock, NowRequest};
use nlos_identity::{IdentityAuthority, IdentityAuthorityError};
use nlos_ipc::handshake::HandshakeError;
use nlos_ipc::handshake::transport::{
    AuthenticatedServeOutcome, ServerHandshakeContext, authenticated_connect,
    authenticated_serve_one,
};
use nlos_ipc::unix::UnixListenerAdapter;
use nlos_ipc::{LocalRpcClient, OutboundResponse, PeerAuthorizer, TransportConfig};
use nlos_schema::sabi::v1::{Envelope, ExchangeRequest, ExchangeResponse, envelope};
use nlos_schema::{HANDSHAKE_NONCE_BYTES, HANDSHAKE_SIGNATURE_BYTES, REQUEST_ID_BYTES};
use nlos_types::{IdempotencyKey, PrincipalId};
use sha2::{Digest, Sha256};

use crate::control::{ControlCommand, ControlError, ControlReceipt, build_request_envelope};
use crate::{
    RecoveryHealthSource, RecoverySystemControl, SystemControlAuthorizer, SystemControlError,
    failure_envelope,
};

/// Domain separator keeping the per-connection handshake verified-at key
/// distinct from every other `AuthorityClock` idempotency domain.
const HANDSHAKE_WALL_DOMAIN: &[u8] = b"llmos/control-auth/handshake-wall/v1";

/// Derives the `AuthorityClock` idempotency key for one control exchange:
/// exactly the request's bounded §25.3 correlation id.
#[must_use]
pub fn command_wall_key(correlation_id: &[u8; REQUEST_ID_BYTES]) -> IdempotencyKey {
    IdempotencyKey::from_bytes(*correlation_id)
}

/// Derives the handshake verified-at key from the one-time server nonce:
/// nonces never repeat, so every connection issues a fresh wall reading.
fn handshake_wall_key(nonce: &[u8; HANDSHAKE_NONCE_BYTES]) -> IdempotencyKey {
    let mut hasher = Sha256::new();
    hasher.update(HANDSHAKE_WALL_DOMAIN);
    hasher.update(nonce);
    let digest: [u8; 32] = hasher.finalize().into();
    let mut key = [0; 16];
    key.copy_from_slice(&digest[..16]);
    IdempotencyKey::from_bytes(key)
}

/// Accepts one connection, gates it with `peer_gate`, runs the ADR-0011
/// challenge-response handshake verified through `identity` at the `clock`'s
/// durable wall reading, and then serves exactly one control exchange with
/// the unchanged [`RecoverySystemControl::handle_for_ipc`] semantics — the
/// exchange's wall time comes from the clock (see [`command_wall_key`]).
///
/// `next_nonce` supplies the one-time server nonce bytes for this single
/// connection; production wiring injects an OS-quality RNG and tests inject
/// deterministic generators. It is consumed exactly once, before the
/// connection is accepted, so even a pre-gate denial leaves only a monotone
/// clock receipt behind.
///
/// # Errors
///
/// Fails closed with the typed [`HandshakeError`] of the underlying
/// transport facility: the connection is dropped before any request byte is
/// dispatched, and the consumed nonce is never returned to the registry.
#[allow(clippy::too_many_arguments)]
pub async fn authenticated_serve_one_control<H, A, P, N>(
    listener: &UnixListenerAdapter,
    config: TransportConfig,
    control: &RecoverySystemControl<'_, H, A>,
    identity: &IdentityAuthority,
    clock: &AuthorityClock,
    handshake: &ServerHandshakeContext,
    peer_gate: &P,
    now_monotonic_ns: u64,
    next_nonce: N,
) -> Result<AuthenticatedServeOutcome, HandshakeError>
where
    H: RecoveryHealthSource,
    A: SystemControlAuthorizer,
    P: PeerAuthorizer,
    N: FnOnce() -> [u8; HANDSHAKE_NONCE_BYTES],
{
    let nonce = next_nonce();
    let verified_at_ms = clock
        .wall_now(NowRequest {
            idempotency_key: handshake_wall_key(&nonce),
        })
        .map_err(|error| HandshakeError::Identity(IdentityAuthorityError::Clock(error)))?
        .reading()
        .as_u64();
    authenticated_serve_one(
        listener,
        config,
        identity,
        handshake.nonces(),
        handshake.binding(),
        peer_gate,
        |validated| async move {
            Ok(OutboundResponse::Typed(ExchangeResponse {
                envelope: Some(serve_validated(
                    control,
                    clock,
                    now_monotonic_ns,
                    validated.envelope(),
                )),
            }))
        },
        move || nonce,
        verified_at_ms,
    )
    .await
}

/// Projects one validated request through the unchanged handler with the
/// clock-issued wall reading; contract violations before the handler are
/// typed failure envelopes from the single sanitizing projection.
fn serve_validated<H, A>(
    control: &RecoverySystemControl<'_, H, A>,
    clock: &AuthorityClock,
    now_monotonic_ns: u64,
    request: &Envelope,
) -> Envelope
where
    H: RecoveryHealthSource,
    A: SystemControlAuthorizer,
{
    let Some(correlation_id) = bounded_correlation(request) else {
        return failure_envelope(request, &SystemControlError::UnboundedCorrelation);
    };
    match clock.wall_now(NowRequest {
        idempotency_key: command_wall_key(&correlation_id),
    }) {
        Ok(decision) => {
            let wall_ms = i64::try_from(decision.reading().as_u64()).unwrap_or(i64::MAX);
            control.handle_for_ipc(request, now_monotonic_ns, wall_ms)
        }
        Err(_) => failure_envelope(request, &SystemControlError::ClockWallUnavailable),
    }
}

fn bounded_correlation(request: &Envelope) -> Option<[u8; REQUEST_ID_BYTES]> {
    match request.common_context.as_ref() {
        Some(envelope::CommonContext::RequestContext(context))
            if context.correlation_id.len() == REQUEST_ID_BYTES =>
        {
            let mut correlation = [0; REQUEST_ID_BYTES];
            correlation.copy_from_slice(&context.correlation_id);
            Some(correlation)
        }
        _ => None,
    }
}

/// Dispatches one [`ControlCommand`] to an ADR-0011 authenticated control
/// endpoint: connects to the Unix socket at `socket`, answers the server's
/// challenge on behalf of `principal` by signing the handshake digest with
/// `sign`, then crosses the same handler path and receipt projection as
/// [`dispatch_over_socket`](crate::control::dispatch_over_socket).
///
/// # Errors
///
/// Returns [`ControlError::Handshake`] for any handshake refusal,
/// [`ControlError::Ipc`] for transport failures, and the schema/projection
/// errors of the plain dispatch path otherwise.
pub async fn dispatch_over_authenticated_socket<S>(
    socket: impl AsRef<Path>,
    principal: PrincipalId,
    sign: S,
    command: &ControlCommand,
) -> Result<ControlReceipt, ControlError>
where
    S: Fn(&[u8; 32]) -> Result<[u8; HANDSHAKE_SIGNATURE_BYTES], HandshakeError>,
{
    let request = build_request_envelope(command)?;
    let config = TransportConfig::default();
    let framed = authenticated_connect(socket, config, principal, sign)
        .await
        .map_err(ControlError::Handshake)?;
    let response = LocalRpcClient::new(framed.into_inner(), config)
        .exchange_validated(ExchangeRequest {
            envelope: Some(request),
        })
        .await
        .map_err(ControlError::Ipc)?;
    ControlReceipt::compose(command, response.envelope())
}
