//! ADR-0011 transport wiring: composes the transport-agnostic handshake
//! primitives with the framed Unix-socket transport.
//!
//! [`authenticated_serve_one`] accepts one connection, runs the
//! OS-credential pre-gate, then the challenge-response handshake, and only
//! then enters the existing [`serve_one`] semantics. [`authenticated_connect`]
//! connects, answers the challenge, and returns a [`FramedIo`] ready for
//! [`LocalRpcClient`](crate::LocalRpcClient) or [`serve_one`].
//!
//! Fail-closed invariants:
//!
//! * Every handshake failure is a typed [`HandshakeError`] and the
//!   connection is dropped before any request byte is dispatched. There is
//!   no session state to clean up.
//! * The one-time nonce is consumed by [`verify_attestation`] *before*
//!   signature verification. A consumed nonce is never returned to the
//!   registry, so a failed handshake burns its nonce by design: a replayed
//!   attestation cannot re-enter verification.
//! * Both ends derive the channel binding from the same endpoint path via
//!   [`endpoint_channel_binding`]; the server verifies against its local
//!   derivation only.
//! * Any frame other than a well-formed attestation received during the
//!   handshake is a typed [`HandshakeError::Schema`] rejection.
//!
//! The server's fresh nonce bytes come from the caller-supplied
//! `next_nonce` generator: this crate deliberately carries no randomness
//! dependency, so production wiring injects an OS-quality RNG and tests
//! inject deterministic generators.

use std::future::Future;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use nlos_identity::IdentityAuthority;
use nlos_schema::{HANDSHAKE_NONCE_BYTES, HANDSHAKE_SIGNATURE_BYTES, ValidatedExchangeRequest};
use nlos_types::PrincipalId;
use sha2::{Digest, Sha256};
use tokio::net::UnixStream;

use super::{
    HandshakeError, HandshakeNonceRegistry, VerifiedPrincipalHandshake, attestation_nonce,
    client_attestation, decode_attestation_wire, decode_challenge_wire, encode_attestation_wire,
    encode_challenge_wire, issue_challenge, principal_handshake_message, verify_attestation,
};
use crate::unix::{UnixListenerAdapter, connect};
use crate::{FramedIo, IpcError, OutboundResponse, PeerAuthorizer, TransportConfig, serve_one};

/// Domain separator keeping endpoint-derived channel bindings distinct from
/// every other SHA-256 domain in the system.
const CHANNEL_BINDING_DOMAIN: &[u8] = b"llmos/ipc-channel-binding/v1";

/// Derives the channel binding both ends of one Unix-socket endpoint agree
/// on: a domain-separated SHA-256 over the endpoint path bytes. The fixed
/// 32-byte output always satisfies the handshake binding bounds regardless
/// of path length. Callers on both ends must pass the same resolved
/// endpoint path (for example the ServiceDirectory-resolved socket path).
#[must_use]
pub fn endpoint_channel_binding(path: &Path) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(CHANNEL_BINDING_DOMAIN);
    hasher.update(path.as_os_str().as_bytes());
    hasher.finalize().into()
}

/// Server-side holder for one authenticated endpoint: the locally derived
/// channel binding plus the one-time nonce registry.
#[derive(Debug)]
pub struct ServerHandshakeContext {
    binding: Vec<u8>,
    nonces: HandshakeNonceRegistry,
}

impl ServerHandshakeContext {
    /// Derives the binding from `endpoint` and creates a registry holding
    /// at most `nonce_capacity` outstanding nonces.
    ///
    /// # Errors
    ///
    /// Returns [`HandshakeError::InvalidConfig`] for a zero capacity.
    pub fn new(endpoint: &Path, nonce_capacity: usize) -> Result<Self, HandshakeError> {
        Ok(Self {
            binding: endpoint_channel_binding(endpoint).to_vec(),
            nonces: HandshakeNonceRegistry::new(nonce_capacity)?,
        })
    }

    #[must_use]
    pub fn binding(&self) -> &[u8] {
        &self.binding
    }

    #[must_use]
    pub const fn nonces(&self) -> &HandshakeNonceRegistry {
        &self.nonces
    }
}

/// Outcome of one [`authenticated_serve_one`] cycle: the principal identity
/// the handshake verified, and the post-handshake serve result.
#[derive(Debug)]
pub struct AuthenticatedServeOutcome {
    verified: VerifiedPrincipalHandshake,
    served: Result<(), IpcError>,
}

impl AuthenticatedServeOutcome {
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

/// Accepts one connection, authorizes its OS credential pre-gate, runs the
/// challenge-response handshake against the caller's `endpoint_binding`,
/// and then serves exactly one exchange with the existing [`serve_one`]
/// semantics. The authorizer deliberately runs twice: once before the
/// handshake (fail fast, zero nonce side effects) and once inside
/// [`serve_one`] (unchanged existing behavior).
///
/// On any handshake failure the connection is closed and the typed
/// [`HandshakeError`] is returned. The one-time nonce is consumed before
/// signature verification and is never returned to the registry, so failed
/// handshakes burn their nonce by design.
///
/// # Errors
///
/// Fails closed with a typed [`HandshakeError`] for pre-gate denial,
/// challenge/attestation transport failures (including timeouts), schema
/// violations (including any non-attestation frame), wrong or replayed
/// nonces, channel-binding drift, unknown principals, revoked or invalid
/// keys, and bad signatures. The post-handshake serve result is reported in
/// [`AuthenticatedServeOutcome::served`], not as a handshake error.
#[allow(clippy::too_many_arguments)]
pub async fn authenticated_serve_one<A, H, F, N>(
    listener: &UnixListenerAdapter,
    config: TransportConfig,
    identity: &IdentityAuthority,
    nonces: &HandshakeNonceRegistry,
    endpoint_binding: &[u8],
    authorizer: &A,
    handler: H,
    mut next_nonce: N,
    verified_at_ms: u64,
) -> Result<AuthenticatedServeOutcome, HandshakeError>
where
    A: PeerAuthorizer,
    H: FnOnce(ValidatedExchangeRequest) -> F,
    F: Future<Output = Result<OutboundResponse, IpcError>>,
    N: FnMut() -> [u8; HANDSHAKE_NONCE_BYTES],
{
    let (stream, peer) = listener.accept(config).await.map_err(transport_failure)?;
    authorizer
        .authorize(&peer)
        .map_err(HandshakeError::PeerAuthorization)?;

    let mut framed = FramedIo::new(stream, config);
    let challenge = issue_challenge(nonces, next_nonce())?;
    framed
        .send(&encode_challenge_wire(&challenge)?)
        .await
        .map_err(transport_failure)?;
    let attestation_wire = framed.receive().await.map_err(transport_failure)?;
    let attestation = decode_attestation_wire(&attestation_wire)?;

    // From here the single-use nonce is consumed even if verification then
    // fails; it is intentionally never returned to the registry.
    let verified = verify_attestation(
        identity,
        nonces,
        &attestation,
        endpoint_binding,
        verified_at_ms,
    )?;

    let served = serve_one(framed.into_inner(), config, peer, authorizer, handler).await;
    Ok(AuthenticatedServeOutcome { verified, served })
}

/// Connects to the Unix socket at `path`, answers the server's challenge on
/// behalf of `principal` by signing the handshake digest through the
/// caller-provided `sign` callback, and returns the framed connection ready
/// for authenticated exchanges.
///
/// The channel binding is derived from `path` via
/// [`endpoint_channel_binding`]; the server must expect the same derivation
/// (see [`ServerHandshakeContext::new`]).
///
/// # Errors
///
/// Fails closed with a typed [`HandshakeError`] for transport failures
/// (including connect/read/write timeouts), malformed or schema-invalid
/// challenges, and signer rejections.
pub async fn authenticated_connect<S>(
    path: impl AsRef<Path>,
    config: TransportConfig,
    principal: PrincipalId,
    sign: S,
) -> Result<FramedIo<UnixStream>, HandshakeError>
where
    S: Fn(&[u8; 32]) -> Result<[u8; HANDSHAKE_SIGNATURE_BYTES], HandshakeError>,
{
    let path = path.as_ref();
    let channel_binding = endpoint_channel_binding(path);
    let (stream, _peer) = connect(path, config).await.map_err(transport_failure)?;
    let mut framed = FramedIo::new(stream, config);

    let challenge_wire = framed.receive().await.map_err(transport_failure)?;
    let challenge = decode_challenge_wire(&challenge_wire)?;
    let nonce = attestation_nonce(&challenge.nonce)?;
    let digest = principal_handshake_message(&nonce, principal, &channel_binding);
    let signature = sign(&digest)?;
    let attestation = client_attestation(principal, &challenge, &channel_binding, signature)?;
    framed
        .send(&encode_attestation_wire(&attestation)?)
        .await
        .map_err(transport_failure)?;
    Ok(framed)
}

fn transport_failure(error: IpcError) -> HandshakeError {
    HandshakeError::Transport(error)
}
