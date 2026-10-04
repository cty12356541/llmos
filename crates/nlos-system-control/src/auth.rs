//! ADR-0011 opt-in authenticated control-plane entry points.
//!
//! Strictly additive over [`crate::control`]: the in-process dispatcher, the
//! plain [`dispatch_over_socket`](crate::control::dispatch_over_socket)
//! route, and every conformance client that depends on the local trust-domain
//! path keep their exact semantics. This module only adds one explicit
//! opt-in service entry, [`authenticated_serve_one_control`], and its
//! matching client, [`dispatch_over_authenticated_socket`].
//!
//! The surface is platform-dispatched exactly like the `nlos-ipc` handshake
//! transport facility it consumes: the Unix-domain socket on Unix and the
//! named pipe on Windows frame the identical handshake and exchange bytes,
//! and only transport acquisition (bind/accept/connect) plus the
//! endpoint-path byte encoding fed into the channel binding differ per
//! platform (see [`EndpointListener`]).
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
#[cfg(unix)]
use nlos_ipc::unix::UnixListenerAdapter;
#[cfg(windows)]
use nlos_ipc::windows::NamedPipeListenerAdapter;
use nlos_ipc::{FramedIo, LocalRpcClient, OutboundResponse, PeerAuthorizer, TransportConfig};
use nlos_schema::sabi::v1::{Envelope, ExchangeRequest, ExchangeResponse, envelope};
use nlos_schema::{HANDSHAKE_NONCE_BYTES, HANDSHAKE_SIGNATURE_BYTES, REQUEST_ID_BYTES};
use nlos_types::{IdempotencyKey, PrincipalId};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
#[cfg(unix)]
use tokio::net::UnixStream;
#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeClient;

use crate::control::{
    ApplicationInspector, ControlCommand, ControlError, ControlReceipt, ProcessInspector,
    ResourceInspector, build_request_envelope,
};
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

/// Platform listener type the authenticated serving entry accepts: the
/// Unix-domain socket listener on Unix, the local named-pipe listener on
/// Windows (whose endpoint path is the pipe path). Both adapters delegate
/// to the same upstream transport core, so only the stream acquisition and
/// the channel-binding path encoding differ — the handshake and exchange
/// wire bytes are identical on every platform.
#[cfg(unix)]
pub type EndpointListener = UnixListenerAdapter;

/// Platform listener type the authenticated serving entry accepts: the
/// Unix-domain socket listener on Unix, the local named-pipe listener on
/// Windows (whose endpoint path is the pipe path). Both adapters delegate
/// to the same upstream transport core, so only the stream acquisition and
/// the channel-binding path encoding differ — the handshake and exchange
/// wire bytes are identical on every platform.
#[cfg(windows)]
pub type EndpointListener = NamedPipeListenerAdapter;

/// Accepts one connection, gates it with `peer_gate`, runs the ADR-0011
/// challenge-response handshake verified through `identity` at the `clock`'s
/// durable wall reading, and then serves exactly one control exchange with
/// the unchanged [`RecoverySystemControl::handle_for_ipc`] semantics — the
/// exchange's wall time comes from the clock (see [`command_wall_key`]).
///
/// The listener is the platform [`EndpointListener`] (a bound Unix-domain
/// socket on Unix, a bound named pipe on Windows) and must be bound to the
/// same endpoint the `handshake` context was derived from.
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
    listener: &mut EndpointListener,
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
///
/// Shared by both served endpoints (F10/W61-A): the authenticated entry
/// above and the daemon's plain entry
/// ([`crate::daemon::serve_plain_endpoint`]) issue their command wall time
/// through this one function, so both faces stamp durable mutation records
/// from the same `AuthorityClock` wall domain under the same
/// [`command_wall_key`] derivation — never from the bare system clock.
pub(crate) fn serve_validated<H, A>(
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
/// endpoint: connects to the platform local endpoint at `socket` — the
/// Unix-domain socket on Unix, the named pipe on Windows — answers the
/// server's challenge on behalf of `principal` by signing the handshake
/// digest with `sign`, then crosses the same handler path and receipt
/// projection as
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
    process: Option<&dyn ProcessInspector>,
    resource: Option<&dyn ResourceInspector>,
    application: Option<&dyn ApplicationInspector>,
) -> Result<ControlReceipt, ControlError>
where
    S: Fn(&[u8; 32]) -> Result<[u8; HANDSHAKE_SIGNATURE_BYTES], HandshakeError>,
{
    let request = build_request_envelope(command)?;
    let config = TransportConfig::default();
    let framed = platform_authenticated_connect(socket, config, principal, sign)
        .await
        .map_err(ControlError::Handshake)?;
    exchange_over_authenticated_stream(
        framed.into_inner(),
        config,
        request,
        command,
        process,
        resource,
        application,
    )
    .await
}

/// Platform dispatch for the authenticated connection acquisition, mirroring
/// the dual-path [`nlos_ipc::handshake::transport::authenticated_connect`]:
/// the Unix-domain socket connect on Unix, the named-pipe connect on
/// Windows. Both wrappers hand the platform stream to the same handshake
/// core, so the handshake bytes and failure order are identical on every
/// platform.
#[cfg(unix)]
async fn platform_authenticated_connect<S>(
    socket: impl AsRef<Path>,
    config: TransportConfig,
    principal: PrincipalId,
    sign: S,
) -> Result<FramedIo<UnixStream>, HandshakeError>
where
    S: Fn(&[u8; 32]) -> Result<[u8; HANDSHAKE_SIGNATURE_BYTES], HandshakeError>,
{
    authenticated_connect(socket, config, principal, sign).await
}

/// Platform dispatch for the authenticated connection acquisition, mirroring
/// the dual-path [`nlos_ipc::handshake::transport::authenticated_connect`]:
/// the Unix-domain socket connect on Unix, the named-pipe connect on
/// Windows. Both wrappers hand the platform stream to the same handshake
/// core, so the handshake bytes and failure order are identical on every
/// platform.
#[cfg(windows)]
async fn platform_authenticated_connect<S>(
    socket: impl AsRef<Path>,
    config: TransportConfig,
    principal: PrincipalId,
    sign: S,
) -> Result<FramedIo<NamedPipeClient>, HandshakeError>
where
    S: Fn(&[u8; 32]) -> Result<[u8; HANDSHAKE_SIGNATURE_BYTES], HandshakeError>,
{
    authenticated_connect(socket, config, principal, sign).await
}

/// Platform-neutral post-handshake exchange core: one validated exchange
/// over the authenticated stream and the shared receipt projection. The
/// Unix path crosses exactly the same calls as before the platform split.
async fn exchange_over_authenticated_stream<St>(
    stream: St,
    config: TransportConfig,
    request: Envelope,
    command: &ControlCommand,
    process: Option<&dyn ProcessInspector>,
    resource: Option<&dyn ResourceInspector>,
    application: Option<&dyn ApplicationInspector>,
) -> Result<ControlReceipt, ControlError>
where
    St: AsyncRead + AsyncWrite + Unpin + Send,
{
    let response = LocalRpcClient::new(stream, config)
        .exchange_validated(ExchangeRequest {
            envelope: Some(request),
        })
        .await
        .map_err(ControlError::Ipc)?;
    ControlReceipt::compose(command, response.envelope(), process, resource, application)
}
