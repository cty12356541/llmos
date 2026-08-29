//! ADR-0011 connection-level principal challenge-response handshake.
//!
//! Minimal prefix: a server registers a one-time nonce, the client answers
//! with an Ed25519 signature over the domain-separated digest of
//! `(nonce, principal id, channel binding)`, and the server verifies it
//! through the [`IdentityAuthority`] against the principal's *current* key
//! binding. Unknown principals, revoked or rotated-away keys, wrong or
//! replayed nonces, tampered signatures, and channel-binding drift all fail
//! closed as typed [`HandshakeError`] values before any session exists.
//!
//! The facility is transport-agnostic; the caller wires it onto any byte
//! stream (for example [`FramedIo`](crate::FramedIo) over a Unix socket):
//!
//! 1. Server: [`issue_challenge`] with a fresh 32-byte nonce, then send
//!    [`encode_challenge_wire`] bytes.
//! 2. Client: [`decode_challenge_wire`], sign [`principal_handshake_message`]
//!    with the principal's Ed25519 private key, then [`client_attestation`]
//!    and send [`encode_attestation_wire`] bytes.
//! 3. Server: [`decode_attestation_wire`], then [`verify_attestation`] with
//!    the locally observed channel binding.
//!
//! The channel binding is caller-derived connection context both ends agree
//! on (for example the resolved endpoint name). The server always verifies
//! against its *local* expectation and rejects any attestation carrying
//! different binding bytes, so client-supplied binding values cannot weaken
//! the pin.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::sync::{Mutex, MutexGuard};

use nlos_identity::{
    IdentityAuthority, IdentityAuthorityError, VerifiedCapabilityCommandSigner,
    VerifyCapabilityCommandSignatureRequest,
};
use nlos_schema::sabi::v1::{PrincipalHandshakeAttestation, PrincipalHandshakeChallenge};
use nlos_schema::{
    CompatibilityError, HANDSHAKE_NONCE_BYTES, HANDSHAKE_SIGNATURE_BYTES,
    MAX_HANDSHAKE_CHANNEL_BINDING_BYTES, decode_principal_handshake_attestation,
    decode_principal_handshake_challenge, encode_principal_handshake_attestation,
    encode_principal_handshake_challenge, principal_handshake_schema_identity,
};
use nlos_types::{ControlDomainId, Generation, KeyId, PrincipalId};
use sha2::{Digest, Sha256};

/// Domain separator prefixing every handshake digest, mirroring the
/// `nlos/capability-*` signed-command message construction.
pub const HANDSHAKE_MESSAGE_DOMAIN: &[u8] = b"llmos/principal-handshake/v1";

#[derive(Debug)]
pub enum HandshakeError {
    InvalidConfig(&'static str),
    NonceRejected,
    NonceCapacityExhausted { capacity: usize },
    ChannelBinding { actual: usize },
    ChannelBindingMismatch,
    MalformedAttestation(&'static str),
    Schema(CompatibilityError),
    SignatureInvalid,
    PrincipalUnknown(PrincipalId),
    KeyRevoked,
    KeyNotYetValid,
    KeyExpired,
    Identity(IdentityAuthorityError),
}

impl fmt::Display for HandshakeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => {
                write!(formatter, "invalid handshake config: {message}")
            }
            Self::NonceRejected => {
                formatter.write_str("handshake nonce is unknown, consumed, or duplicate")
            }
            Self::NonceCapacityExhausted { capacity } => write!(
                formatter,
                "handshake nonce registry is exhausted at {capacity} outstanding nonces"
            ),
            Self::ChannelBinding { actual } => write!(
                formatter,
                "handshake channel binding has {actual} bytes; 1..={MAX_HANDSHAKE_CHANNEL_BINDING_BYTES} are required"
            ),
            Self::ChannelBindingMismatch => formatter
                .write_str("attestation channel binding does not match the local connection"),
            Self::MalformedAttestation(reason) => {
                write!(formatter, "handshake attestation is malformed: {reason}")
            }
            Self::Schema(error) => write!(formatter, "handshake schema violation: {error}"),
            Self::SignatureInvalid => formatter.write_str("handshake signature is invalid"),
            Self::PrincipalUnknown(id) => write!(formatter, "principal {id:?} does not exist"),
            Self::KeyRevoked => formatter.write_str("handshake signing key is revoked"),
            Self::KeyNotYetValid => formatter.write_str("handshake signing key is not yet valid"),
            Self::KeyExpired => formatter.write_str("handshake signing key has expired"),
            Self::Identity(error) => write!(formatter, "identity authority failure: {error}"),
        }
    }
}

impl Error for HandshakeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Schema(error) => Some(error),
            Self::Identity(error) => Some(error),
            _ => None,
        }
    }
}

/// Computes the exact digest a client signs and the server verifies: the
/// domain separator, the fixed-width nonce and principal id, and the
/// variable-length channel binding as the final field (so concatenation is
/// unambiguous without extra framing).
#[must_use]
pub fn principal_handshake_message(
    nonce: &[u8; HANDSHAKE_NONCE_BYTES],
    principal: PrincipalId,
    channel_binding: &[u8],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(HANDSHAKE_MESSAGE_DOMAIN);
    hasher.update(nonce);
    hasher.update(principal.as_bytes());
    hasher.update(channel_binding);
    hasher.finalize().into()
}

/// Bounded one-time nonce registry. The server supplies its own fresh nonce
/// bytes; this registry only enforces uniqueness at issue time and single
/// use at consume time, so replays and retry oracles fail closed.
#[derive(Debug)]
pub struct HandshakeNonceRegistry {
    capacity: usize,
    issued: Mutex<HashSet<[u8; HANDSHAKE_NONCE_BYTES]>>,
}

impl HandshakeNonceRegistry {
    /// Creates a registry holding at most `capacity` outstanding nonces.
    ///
    /// # Errors
    ///
    /// Rejects a zero capacity.
    pub fn new(capacity: usize) -> Result<Self, HandshakeError> {
        if capacity == 0 {
            return Err(HandshakeError::InvalidConfig(
                "nonce registry capacity must be non-zero",
            ));
        }
        Ok(Self {
            capacity,
            issued: Mutex::new(HashSet::new()),
        })
    }

    /// Registers one fresh server-generated nonce. Duplicate registration is
    /// rejected instead of silently refreshed.
    ///
    /// # Errors
    ///
    /// Fails closed for duplicate nonces, exhausted capacity, or a poisoned
    /// registry lock.
    pub fn register(&self, nonce: [u8; HANDSHAKE_NONCE_BYTES]) -> Result<(), HandshakeError> {
        let mut issued = self.lock()?;
        if !issued.contains(&nonce) && issued.len() >= self.capacity {
            return Err(HandshakeError::NonceCapacityExhausted {
                capacity: self.capacity,
            });
        }
        if issued.insert(nonce) {
            Ok(())
        } else {
            Err(HandshakeError::NonceRejected)
        }
    }

    /// Consumes a nonce exactly once.
    ///
    /// # Errors
    ///
    /// Returns [`HandshakeError::NonceRejected`] for unknown or already
    /// consumed nonces.
    pub fn consume(&self, nonce: &[u8; HANDSHAKE_NONCE_BYTES]) -> Result<(), HandshakeError> {
        if self.lock()?.remove(nonce) {
            Ok(())
        } else {
            Err(HandshakeError::NonceRejected)
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, HashSet<[u8; HANDSHAKE_NONCE_BYTES]>>, HandshakeError> {
        self.issued
            .lock()
            .map_err(|_| HandshakeError::InvalidConfig("nonce registry lock is poisoned"))
    }
}

/// The durable binding that authenticated one handshake, mirroring the
/// verified-signer surfaces of `nlos-identity`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerifiedPrincipalHandshake {
    principal_id: PrincipalId,
    control_domain_id: ControlDomainId,
    key_id: KeyId,
    key_generation: Generation,
}

impl VerifiedPrincipalHandshake {
    #[must_use]
    pub const fn principal_id(self) -> PrincipalId {
        self.principal_id
    }

    #[must_use]
    pub const fn control_domain_id(self) -> ControlDomainId {
        self.control_domain_id
    }

    #[must_use]
    pub const fn key_id(self) -> KeyId {
        self.key_id
    }

    #[must_use]
    pub const fn key_generation(self) -> Generation {
        self.key_generation
    }
}

impl From<VerifiedCapabilityCommandSigner> for VerifiedPrincipalHandshake {
    fn from(signer: VerifiedCapabilityCommandSigner) -> Self {
        Self {
            principal_id: signer.principal_id(),
            control_domain_id: signer.control_domain_id(),
            key_id: signer.key_id(),
            key_generation: signer.key_generation(),
        }
    }
}

/// Registers the server's fresh nonce and returns the typed challenge.
///
/// # Errors
///
/// Fails closed on duplicate nonce, exhausted registry, or config errors.
pub fn issue_challenge(
    registry: &HandshakeNonceRegistry,
    nonce: [u8; HANDSHAKE_NONCE_BYTES],
) -> Result<PrincipalHandshakeChallenge, HandshakeError> {
    registry.register(nonce)?;
    Ok(PrincipalHandshakeChallenge {
        schema: Some(principal_handshake_schema_identity()),
        nonce: nonce.to_vec(),
    })
}

/// Builds the client's attestation for one received challenge. The caller
/// signs [`principal_handshake_message`] with the principal's Ed25519
/// private key and passes the signature; `channel_binding` must be the exact
/// binding the server will locally expect (for example the resolved endpoint
/// name).
///
/// # Errors
///
/// Fails closed for a malformed challenge nonce or an empty/oversized
/// channel binding.
pub fn client_attestation(
    principal: PrincipalId,
    challenge: &PrincipalHandshakeChallenge,
    channel_binding: &[u8],
    signature: [u8; HANDSHAKE_SIGNATURE_BYTES],
) -> Result<PrincipalHandshakeAttestation, HandshakeError> {
    validate_channel_binding(channel_binding)?;
    let nonce = attestation_nonce(&challenge.nonce)?;
    Ok(PrincipalHandshakeAttestation {
        schema: Some(principal_handshake_schema_identity()),
        principal_id: principal.as_bytes().to_vec(),
        nonce: nonce.to_vec(),
        channel_binding: channel_binding.to_vec(),
        signature: signature.to_vec(),
    })
}

/// Verifies one attestation against the server's locally observed channel
/// binding and the principal's current `IdentityAuthority` key binding.
///
/// The nonce is consumed on the first verification attempt, so a replayed
/// attestation fails closed even when its signature was valid. Every failure
/// path is a typed error and leaves no session state behind.
///
/// # Errors
///
/// Returns a typed fail-closed [`HandshakeError`] for binding drift, wrong
/// or replayed nonces, unknown principals, revoked/invalid keys, and bad
/// signatures.
pub fn verify_attestation(
    identity: &IdentityAuthority,
    registry: &HandshakeNonceRegistry,
    attestation: &PrincipalHandshakeAttestation,
    local_channel_binding: &[u8],
    verified_at_ms: u64,
) -> Result<VerifiedPrincipalHandshake, HandshakeError> {
    validate_channel_binding(local_channel_binding)?;
    if attestation.channel_binding != local_channel_binding {
        return Err(HandshakeError::ChannelBindingMismatch);
    }
    let nonce = attestation_nonce(&attestation.nonce)?;
    let principal = attestation_principal(&attestation.principal_id)?;
    let signature: [u8; HANDSHAKE_SIGNATURE_BYTES] = attestation
        .signature
        .as_slice()
        .try_into()
        .map_err(|_| HandshakeError::MalformedAttestation("signature length"))?;

    registry.consume(&nonce)?;
    let message_digest = principal_handshake_message(&nonce, principal, local_channel_binding);
    identity
        .verify_capability_command_signature(VerifyCapabilityCommandSignatureRequest {
            message_digest,
            principal,
            signature,
            verified_at_ms,
        })
        .map(VerifiedPrincipalHandshake::from)
        .map_err(identity_error)
}

/// Lifts identity failures into the typed handshake surface, mirroring the
/// capability-command error mapping; all other identity failures keep their
/// typed wrapping.
fn identity_error(error: IdentityAuthorityError) -> HandshakeError {
    match error {
        IdentityAuthorityError::InvalidSignature => HandshakeError::SignatureInvalid,
        IdentityAuthorityError::PrincipalNotFound(id) => HandshakeError::PrincipalUnknown(id),
        IdentityAuthorityError::KeyRevoked => HandshakeError::KeyRevoked,
        IdentityAuthorityError::KeyNotYetValid => HandshakeError::KeyNotYetValid,
        IdentityAuthorityError::KeyExpired => HandshakeError::KeyExpired,
        other => HandshakeError::Identity(other),
    }
}

fn validate_channel_binding(channel_binding: &[u8]) -> Result<(), HandshakeError> {
    if channel_binding.is_empty() || channel_binding.len() > MAX_HANDSHAKE_CHANNEL_BINDING_BYTES {
        return Err(HandshakeError::ChannelBinding {
            actual: channel_binding.len(),
        });
    }
    Ok(())
}

fn attestation_nonce(bytes: &[u8]) -> Result<[u8; HANDSHAKE_NONCE_BYTES], HandshakeError> {
    bytes
        .try_into()
        .map_err(|_| HandshakeError::MalformedAttestation("nonce length"))
}

fn attestation_principal(bytes: &[u8]) -> Result<PrincipalId, HandshakeError> {
    let bytes: [u8; 16] = bytes
        .try_into()
        .map_err(|_| HandshakeError::MalformedAttestation("principal id length"))?;
    Ok(PrincipalId::from_bytes(bytes))
}

/// Encodes bounded, validated challenge wire bytes.
///
/// # Errors
///
/// Returns [`HandshakeError::Schema`] for codec violations.
pub fn encode_challenge_wire(
    challenge: &PrincipalHandshakeChallenge,
) -> Result<Vec<u8>, HandshakeError> {
    encode_principal_handshake_challenge(challenge).map_err(HandshakeError::Schema)
}

/// Decodes bounded, validated challenge wire bytes.
///
/// # Errors
///
/// Returns [`HandshakeError::Schema`] for codec violations.
pub fn decode_challenge_wire(wire: &[u8]) -> Result<PrincipalHandshakeChallenge, HandshakeError> {
    decode_principal_handshake_challenge(wire).map_err(HandshakeError::Schema)
}

/// Encodes bounded, validated attestation wire bytes.
///
/// # Errors
///
/// Returns [`HandshakeError::Schema`] for codec violations.
pub fn encode_attestation_wire(
    attestation: &PrincipalHandshakeAttestation,
) -> Result<Vec<u8>, HandshakeError> {
    encode_principal_handshake_attestation(attestation).map_err(HandshakeError::Schema)
}

/// Decodes bounded, validated attestation wire bytes.
///
/// # Errors
///
/// Returns [`HandshakeError::Schema`] for codec violations.
pub fn decode_attestation_wire(
    wire: &[u8],
) -> Result<PrincipalHandshakeAttestation, HandshakeError> {
    decode_principal_handshake_attestation(wire).map_err(HandshakeError::Schema)
}
