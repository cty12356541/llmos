//! 5-Axis TLS 1.3 Exporter (`RFC 8446` / `RFC 9266`) bidirectional `CellFence`
//! peer authentication handshake (`[TCB-ENDPOINT-001]`, `[DIST-NAME-001]`,
//! `ADR-0026` §2.2).
//!
//! Immediately after TLS 1.3 mTLS completes (and BEFORE any `ControlRpc` or
//! `BulkChunk` frame may be exchanged), both endpoints extract 32 bytes of
//! per-connection cryptographic keying material via
//! `CommonState::export_keying_material` under label
//! `"EXPORTER-nlos-cell-fence-v1"` and complete a mutual Ed25519
//! challenge-response binding all five required axes:
//! `(CellIdentity, node_boot_generation, PrincipalId, CellEpoch, CellFencingToken)`.

use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::io;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use nlos_cell::{CellEpoch, CellFence, CellFencingToken, CellIdentity};
use nlos_types::{ControlDomainId, Generation, KeyId, PrincipalId};
use rustls::pki_types::CertificateDer;
use rustls::{CommonState, ConnectionCommon};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

use crate::cert::{CertVerifyError, verify_node_x509_cert_der};
use crate::identity::{IdentityPeerVerifier, PeerDigestSigner, PeerIdentityError};
use crate::resolver::EndpointResolver;

/// Normative TLS 1.3 Exporter label (`ADR-0026` §2.2, `RFC 8446 §7.5`).
pub const TLS_EXPORTER_LABEL: &[u8] = b"EXPORTER-nlos-cell-fence-v1";

/// Normative TLS 1.3 Exporter context string.
pub const TLS_EXPORTER_CONTEXT: &[u8] = b"llmos/cell-mtls-exporter-context/v1";

/// Byte width of the extracted TLS 1.3 Exporter secret.
pub const TLS_EXPORTER_LEN: usize = 32;

/// Domain separator for the 5-axis TLS Exporter challenge-response digest.
pub const CELL_FENCE_HANDSHAKE_DOMAIN: &[u8] = b"llmos/net/cell-fence-exporter-handshake/v1";

const HANDSHAKE_MAX_FRAME_BYTES: usize = 1024;
/// Wire tag for step 1 `ClientHelloFence(client_nonce, client_claims)`.
pub const TAG_CLIENT_HELLO: u8 = 0x01;
/// Wire tag for step 2 `ServerChallengeAttestation(server_nonce, server_claims, server_sig)`.
pub const TAG_SERVER_CHALLENGE_ATTEST: u8 = 0x02;
/// Wire tag for step 3 `ClientFenceAttestation(client_sig)`.
pub const TAG_CLIENT_ATTEST: u8 = 0x03;
/// Wire tag for step 4 `ServerReadyAck(session_transcript_digest)`.
pub const TAG_SERVER_READY_ACK: u8 = 0x04;
/// Wire tag for explicit handshake rejection `(TAG_HANDSHAKE_REJECT, HandshakeRejectCode)`.
pub const TAG_HANDSHAKE_REJECT: u8 = 0x7f;

static NONCE_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Role tag domain-separating server vs client handshake signatures so a peer
/// can never reflect its counterpart's attestation back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum HandshakeRole {
    /// Initiating connector (`Client -> Server` attestation).
    Client = 1,
    /// Accepting listener (`Server -> Client` attestation).
    Server = 2,
}

/// Five-axis peer identity & fence presentation bound to a TLS 1.3 session
/// (`ADR-0026` §2.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerFenceClaims {
    /// Axis 1: Nominal 16-byte `CellIdentity` (`[DIST-NAME-001]`).
    pub cell_identity: CellIdentity,
    /// Axis 2: Process boot incarnation `node_boot_generation`.
    pub node_boot_generation: Generation,
    /// Axis 3: Authenticated node `PrincipalId`.
    pub principal_id: PrincipalId,
    /// Axis 4: Monotonic `CellEpoch`.
    pub cell_epoch: CellEpoch,
    /// Axis 5: Monotonic `CellFencingToken`.
    pub cell_fencing_token: CellFencingToken,
    /// Active custody `KeyId` of `principal_id`.
    pub key_id: KeyId,
    /// Active custody key generation (`rotation_epoch`) of `key_id`.
    pub key_generation: Generation,
}

impl PeerFenceClaims {
    /// Constructs a 5-axis [`PeerFenceClaims`] from a [`CellFence`] and active
    /// [`PeerDigestSigner`] metadata.
    #[must_use]
    pub fn from_fence_and_signer(fence: CellFence, signer: &dyn PeerDigestSigner) -> Self {
        Self {
            cell_identity: fence.identity(),
            node_boot_generation: fence.node_boot_generation(),
            principal_id: signer.principal_id(),
            cell_epoch: fence.epoch(),
            cell_fencing_token: fence.fencing_token(),
            key_id: signer.key_id(),
            key_generation: signer.key_generation(),
        }
    }

    /// Projects the 4-axis [`CellFence`] carried by these claims.
    #[must_use]
    pub const fn cell_fence(&self) -> CellFence {
        CellFence::present(
            self.cell_identity,
            self.node_boot_generation,
            self.cell_epoch,
            self.cell_fencing_token,
        )
    }

    /// Encodes the 80-byte big-endian wire representation of these 5-axis claims.
    pub fn encode_into(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(self.cell_identity.as_bytes());
        buf.extend_from_slice(&self.node_boot_generation.get().to_be_bytes());
        buf.extend_from_slice(self.principal_id.as_bytes());
        buf.extend_from_slice(&self.cell_epoch.get().to_be_bytes());
        buf.extend_from_slice(&self.cell_fencing_token.get().to_be_bytes());
        buf.extend_from_slice(self.key_id.as_bytes());
        buf.extend_from_slice(&self.key_generation.get().to_be_bytes());
    }

    /// Decodes an exact 80-byte big-endian wire representation of [`PeerFenceClaims`].
    ///
    /// # Errors
    ///
    /// Fails closed on length mismatch or zero `Generation` / `CellEpoch` /
    /// `CellFencingToken`.
    #[allow(clippy::missing_panics_doc)]
    pub fn decode_exact(bytes: &[u8]) -> Result<Self, FenceHandshakeError> {
        if bytes.len() != 80 {
            return Err(FenceHandshakeError::MalformedWire(
                "PeerFenceClaims length != 80",
            ));
        }
        let cell_bytes: [u8; 16] = bytes[0..16].try_into().expect("16B");
        let boot_raw = u64::from_be_bytes(bytes[16..24].try_into().expect("8B"));
        let principal_bytes: [u8; 16] = bytes[24..40].try_into().expect("16B");
        let epoch_raw = u64::from_be_bytes(bytes[40..48].try_into().expect("8B"));
        let token_raw = u64::from_be_bytes(bytes[48..56].try_into().expect("8B"));
        let key_bytes: [u8; 16] = bytes[56..72].try_into().expect("16B");
        let key_gen_raw = u64::from_be_bytes(bytes[72..80].try_into().expect("8B"));

        let node_boot_generation = NonZeroU64::new(boot_raw)
            .map(Generation::new)
            .ok_or(FenceHandshakeError::MalformedWire("zero boot generation"))?;
        let cell_epoch = CellEpoch::from_u64(epoch_raw)
            .ok_or(FenceHandshakeError::MalformedWire("zero cell_epoch"))?;
        let cell_fencing_token = CellFencingToken::from_u64(token_raw).ok_or(
            FenceHandshakeError::MalformedWire("zero cell_fencing_token"),
        )?;
        let key_generation = NonZeroU64::new(key_gen_raw)
            .map(Generation::new)
            .ok_or(FenceHandshakeError::MalformedWire("zero key_generation"))?;

        Ok(Self {
            cell_identity: CellIdentity::from_domain(nlos_types::SchedulerDomainId::from_bytes(
                cell_bytes,
            )),
            node_boot_generation,
            principal_id: PrincipalId::from_bytes(principal_bytes),
            cell_epoch,
            cell_fencing_token,
            key_id: KeyId::from_bytes(key_bytes),
            key_generation,
        })
    }
}

/// Computes the exact 32-byte digest signed and verified for one direction of
/// the 5-axis TLS 1.3 Exporter handshake.
#[must_use]
pub fn compute_fence_exporter_digest(
    signer_role: HandshakeRole,
    tls_exporter: &[u8; TLS_EXPORTER_LEN],
    client_nonce: &[u8; 32],
    server_nonce: &[u8; 32],
    signer_claims: &PeerFenceClaims,
    counterparty_cell: CellIdentity,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(CELL_FENCE_HANDSHAKE_DOMAIN);
    hasher.update([signer_role as u8]);
    hasher.update(tls_exporter);
    hasher.update(client_nonce);
    hasher.update(server_nonce);
    let mut claim_bytes = Vec::with_capacity(80);
    signer_claims.encode_into(&mut claim_bytes);
    hasher.update(&claim_bytes);
    hasher.update(counterparty_cell.as_bytes());
    hasher.finalize().into()
}

/// Pre-extracted TLS 1.3 peer certificate and RFC 8446 / RFC 9266 32-byte
/// Exporter secret from an established `rustls` [`CommonState`].
///
/// Extracting this value before borrowing `TlsStream` mutably for async I/O
/// avoids simultaneous shared + mutable borrow conflicts on `TlsStream`.
#[derive(Clone, Debug)]
pub struct ExtractedTls13Material {
    /// 32-byte TLS 1.3 Exporter secret derived with [`TLS_EXPORTER_LABEL`].
    pub exporter: [u8; TLS_EXPORTER_LEN],
    /// Leaf peer X.509 DER certificate presented during mTLS.
    pub peer_cert_der: CertificateDer<'static>,
}

/// Extracts [`ExtractedTls13Material`] from an established `rustls`
/// [`ConnectionCommon`], failing closed if TLS 1.3 was not negotiated or no
/// peer certificate is present.
///
/// # Errors
///
/// Fails closed if TLS 1.3 was not negotiated, if exporter derivation fails, or
/// if no peer X.509 certificate was presented.
pub fn extract_tls13_material<D>(
    tls_conn: &ConnectionCommon<D>,
) -> Result<ExtractedTls13Material, FenceHandshakeError> {
    let exporter = extract_tls13_fence_exporter(tls_conn)?;
    let peer_cert_der = first_peer_cert(tls_conn)?.clone();
    Ok(ExtractedTls13Material {
        exporter,
        peer_cert_der,
    })
}

/// Extracts the 32-byte RFC 8446 / RFC 9266 TLS 1.3 Exporter secret from an
/// established `rustls` [`ConnectionCommon`], failing closed if the negotiated
/// protocol version is not strictly TLS 1.3.
///
/// # Errors
///
/// Fails closed if TLS 1.3 was not negotiated or if exporter derivation fails.
pub fn extract_tls13_fence_exporter<D>(
    tls_conn: &ConnectionCommon<D>,
) -> Result<[u8; TLS_EXPORTER_LEN], FenceHandshakeError> {
    if tls_conn.protocol_version() != Some(rustls::ProtocolVersion::TLSv1_3) {
        return Err(FenceHandshakeError::ProtocolNotTls13);
    }
    let mut exporter = [0u8; TLS_EXPORTER_LEN];
    tls_conn
        .export_keying_material(
            &mut exporter,
            TLS_EXPORTER_LABEL,
            Some(TLS_EXPORTER_CONTEXT),
        )
        .map_err(|_| FenceHandshakeError::ExporterUnavailable)?;
    Ok(exporter)
}

/// Policy gate enforcing `CellIdentity <-> PrincipalId` binding and monotonic
/// `(node_boot_generation, cell_epoch, cell_fencing_token)` admission on
/// incoming and outgoing handshakes (`[TCB-ENDPOINT-001]`, `[DIST-NAME-001]`).
pub trait PeerFencePolicy: Send + Sync {
    /// Read-only pre-signature check verifying that `peer_claims` satisfies
    /// known `CellIdentity <-> PrincipalId` bindings and minimum monotonic
    /// `(node_boot_generation, cell_epoch, cell_fencing_token)` floors.
    ///
    /// Implementations MUST NOT mutate internal watermarks or pins here so an
    /// unauthenticated Step 1 `ClientHelloFence` (e.g. `node_boot_generation = u64::MAX`)
    /// can never poison monotonic state prior to signature verification.
    ///
    /// # Errors
    ///
    /// Returns [`HandshakeRejectCode`] (`CellPrincipalBindingMismatch`,
    /// `StaleBootGeneration`, or `StaleCellEpoch`) when `peer_claims` violates
    /// pinned principal bindings or regresses below known floors.
    fn check_peer_fence(&self, peer_claims: &PeerFenceClaims) -> Result<(), HandshakeRejectCode>;

    /// Atomically re-validates `peer_claims` and commits the verified
    /// `(PrincipalId, node_boot_generation, cell_epoch, cell_fencing_token)`
    /// watermark strictly AFTER cryptographic TLS 1.3 Exporter signature
    /// verification has succeeded.
    ///
    /// # Errors
    ///
    /// Returns [`HandshakeRejectCode`] if a concurrent verified session pinned a
    /// conflicting `PrincipalId` or advanced the monotonic floor past `peer_claims`.
    fn commit_verified_peer_fence(
        &self,
        peer_claims: &PeerFenceClaims,
    ) -> Result<(), HandshakeRejectCode>;
}

/// Compact wire-encodable rejection code returned when a peer fails handshake
/// validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum HandshakeRejectCode {
    /// Peer `PrincipalId` in 5-axis claims disagreed with its TLS 1.3 X.509
    /// certificate.
    CertificatePrincipalMismatch = 1,
    /// Peer `node_boot_generation` is older than the minimum known boot
    /// generation for `CellIdentity`.
    StaleBootGeneration = 2,
    /// Peer `cell_epoch` or `cell_fencing_token` is older than the minimum
    /// known epoch floor for `CellIdentity`.
    StaleCellEpoch = 3,
    /// Peer `PrincipalId` is unknown to `IdentityAuthority`.
    PrincipalUnknown = 4,
    /// Peer signing key is revoked, compromised, or rotated away.
    KeyRevokedOrCompromised = 5,
    /// Peer signature failed verification over this connection's TLS 1.3
    /// Exporter secret (tampered or replayed from another TLS session).
    ExporterSignatureInvalid = 6,
    /// Replayed or unknown handshake nonce.
    NonceRejected = 7,
    /// Self-loop identity collision (`peer.cell_identity == local.cell_identity`).
    SelfIdentityCollision = 8,
    /// Authenticated peer `PrincipalId` disagreed with the authoritative
    /// `PrincipalId` pinned for `peer.cell_identity` (sibling-node masquerade).
    CellPrincipalBindingMismatch = 9,
}

impl HandshakeRejectCode {
    const fn from_u8(code: u8) -> Self {
        match code {
            1 => Self::CertificatePrincipalMismatch,
            2 => Self::StaleBootGeneration,
            3 => Self::StaleCellEpoch,
            4 => Self::PrincipalUnknown,
            5 => Self::KeyRevokedOrCompromised,
            7 => Self::NonceRejected,
            8 => Self::SelfIdentityCollision,
            9 => Self::CellPrincipalBindingMismatch,
            _ => Self::ExporterSignatureInvalid,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CellFenceWatermarkEntry {
    expected_principal: Option<PrincipalId>,
    min_fence: Option<(Generation, CellEpoch, CellFencingToken)>,
}

/// Default monotonic [`PeerFencePolicy`] tracking per-Cell `PrincipalId`
/// bindings and `(node_boot_generation, cell_epoch, cell_fencing_token)`
/// watermarks, optionally cross-checking an [`EndpointResolver`].
pub struct MonotonicPeerFenceGate {
    entries: Mutex<BTreeMap<CellIdentity, CellFenceWatermarkEntry>>,
    resolver: Option<Arc<dyn EndpointResolver>>,
}

impl fmt::Debug for MonotonicPeerFenceGate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MonotonicPeerFenceGate")
            .field("has_resolver", &self.resolver.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for MonotonicPeerFenceGate {
    fn default() -> Self {
        Self::new(None)
    }
}

impl MonotonicPeerFenceGate {
    /// Creates a monotonic peer fence gate, optionally consulting `resolver`
    /// for already-observed `(PrincipalId, node_boot_generation, epoch)` floors.
    #[must_use]
    pub fn new(resolver: Option<Arc<dyn EndpointResolver>>) -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
            resolver,
        }
    }

    /// Pins the authoritative `PrincipalId` expected for `cell`.
    ///
    /// # Errors
    ///
    /// Returns [`HandshakeRejectCode::CellPrincipalBindingMismatch`] if `cell`
    /// is already pinned to a different `PrincipalId`.
    pub fn pin_cell_principal(
        &self,
        cell: CellIdentity,
        principal: PrincipalId,
    ) -> Result<(), HandshakeRejectCode> {
        let mut guard = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = guard.entry(cell).or_default();
        if let Some(existing) = entry.expected_principal
            && existing != principal
        {
            return Err(HandshakeRejectCode::CellPrincipalBindingMismatch);
        }
        entry.expected_principal = Some(principal);
        Ok(())
    }

    /// Returns the currently pinned `PrincipalId` for `cell`, if any.
    #[must_use]
    pub fn expected_principal_for(&self, cell: CellIdentity) -> Option<PrincipalId> {
        let guard = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        guard.get(&cell).and_then(|e| e.expected_principal)
    }

    /// Pins a minimum required `(node_boot_generation, cell_epoch,
    /// cell_fencing_token)` floor for `cell`.
    pub fn pin_minimum_fence(
        &self,
        cell: CellIdentity,
        min_boot_generation: Generation,
        min_epoch: CellEpoch,
        min_token: CellFencingToken,
    ) {
        let mut guard = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = guard.entry(cell).or_default();
        let candidate = (min_boot_generation, min_epoch, min_token);
        if entry.min_fence.is_none_or(|cur| candidate > cur) {
            entry.min_fence = Some(candidate);
        }
    }

    fn check_against_resolver(
        &self,
        peer_claims: &PeerFenceClaims,
    ) -> Result<(), HandshakeRejectCode> {
        if let Some(resolver) = &self.resolver
            && let Some(hint) = resolver.resolve(peer_claims.cell_identity)
        {
            if let Some(expected_p) = hint.expected_principal
                && peer_claims.principal_id != expected_p
            {
                return Err(HandshakeRejectCode::CellPrincipalBindingMismatch);
            }
            if peer_claims.node_boot_generation < hint.version.node_boot_generation {
                return Err(HandshakeRejectCode::StaleBootGeneration);
            }
            if peer_claims.node_boot_generation == hint.version.node_boot_generation
                && peer_claims.cell_epoch < hint.version.epoch
            {
                return Err(HandshakeRejectCode::StaleCellEpoch);
            }
        }
        Ok(())
    }

    fn validate_entry(
        entry: &CellFenceWatermarkEntry,
        peer_claims: &PeerFenceClaims,
    ) -> Result<(), HandshakeRejectCode> {
        if let Some(expected_p) = entry.expected_principal
            && peer_claims.principal_id != expected_p
        {
            return Err(HandshakeRejectCode::CellPrincipalBindingMismatch);
        }
        if let Some((min_boot, min_epoch, min_token)) = entry.min_fence {
            if peer_claims.node_boot_generation < min_boot {
                return Err(HandshakeRejectCode::StaleBootGeneration);
            }
            if peer_claims.node_boot_generation == min_boot {
                if peer_claims.cell_epoch < min_epoch {
                    return Err(HandshakeRejectCode::StaleCellEpoch);
                }
                if peer_claims.cell_epoch == min_epoch && peer_claims.cell_fencing_token < min_token
                {
                    return Err(HandshakeRejectCode::StaleCellEpoch);
                }
            }
        }
        Ok(())
    }
}

impl PeerFencePolicy for MonotonicPeerFenceGate {
    fn check_peer_fence(&self, peer_claims: &PeerFenceClaims) -> Result<(), HandshakeRejectCode> {
        self.check_against_resolver(peer_claims)?;
        let guard = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = guard.get(&peer_claims.cell_identity) {
            Self::validate_entry(entry, peer_claims)?;
        }
        Ok(())
    }

    fn commit_verified_peer_fence(
        &self,
        peer_claims: &PeerFenceClaims,
    ) -> Result<(), HandshakeRejectCode> {
        self.check_against_resolver(peer_claims)?;
        let mut guard = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = guard.get(&peer_claims.cell_identity) {
            Self::validate_entry(entry, peer_claims)?;
        }
        let entry = guard.entry(peer_claims.cell_identity).or_default();
        entry.expected_principal = Some(peer_claims.principal_id);
        entry.min_fence = Some((
            peer_claims.node_boot_generation,
            peer_claims.cell_epoch,
            peer_claims.cell_fencing_token,
        ));
        Ok(())
    }
}

/// Bounded anti-DoS server nonce ring (oldest-first displacement, mirroring
/// `nlos-ipc::handshake::HandshakeNonceRegistry`).
#[derive(Debug)]
pub struct FenceNonceRegistry {
    capacity: usize,
    seen: Mutex<VecDeque<[u8; 32]>>,
}

impl Default for FenceNonceRegistry {
    fn default() -> Self {
        Self::new(256)
    }
}

impl FenceNonceRegistry {
    /// Creates a bounded nonce replay filter with `capacity >= 1`.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            seen: Mutex::new(VecDeque::new()),
        }
    }

    /// Generates a fresh unique 32-byte nonce mixed from process/thread address,
    /// monotonic counter, and `local_cell` identity.
    #[must_use]
    pub fn issue_fresh_nonce(&self, local_cell: CellIdentity, salt: &[u8]) -> [u8; 32] {
        let seq = NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut hasher = Sha256::new();
        hasher.update(b"llmos/net/fence-handshake-nonce/v1");
        hasher.update(local_cell.as_bytes());
        hasher.update(std::process::id().to_be_bytes());
        hasher.update(seq.to_be_bytes());
        hasher.update(salt);
        let nonce: [u8; 32] = hasher.finalize().into();

        let mut guard = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.len() >= self.capacity {
            let _ = guard.pop_front();
        }
        guard.push_back(nonce);
        nonce
    }

    /// Consumes a previously issued nonce exactly once.
    ///
    /// # Errors
    ///
    /// Returns [`FenceHandshakeError::RejectedByLocal`] with
    /// [`HandshakeRejectCode::NonceRejected`] if `nonce` is unknown or already
    /// consumed.
    pub fn consume_once(&self, nonce: &[u8; 32]) -> Result<(), FenceHandshakeError> {
        let mut guard = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(pos) = guard.iter().position(|n| n == nonce) {
            guard.remove(pos);
            Ok(())
        } else {
            Err(FenceHandshakeError::RejectedByLocal(
                HandshakeRejectCode::NonceRejected,
            ))
        }
    }
}

/// Pinned cryptographic session context established upon successful completion
/// of TLS 1.3 mTLS + 5-axis TLS Exporter challenge-response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthenticatedPeerSession {
    /// Local node's 5-axis claims presented on this connection.
    pub local_claims: PeerFenceClaims,
    /// Authenticated remote peer's 5-axis claims pinned to this connection.
    pub peer_claims: PeerFenceClaims,
    /// Authoritative `ControlDomainId` of `peer_claims.principal_id`.
    pub peer_control_domain: ControlDomainId,
    /// 32-byte RFC 8446 TLS 1.3 Exporter secret (`"EXPORTER-nlos-cell-fence-v1"`).
    pub tls_exporter_binding: [u8; TLS_EXPORTER_LEN],
    /// Domain-separated `SHA-256` transcript digest binding both nonces,
    /// exporter secret, and both 5-axis claims.
    pub session_transcript_digest: [u8; 32],
}

/// Fail-closed errors emitted during the 5-axis TLS 1.3 Exporter handshake.
#[derive(Debug)]
pub enum FenceHandshakeError {
    /// Underlying connection did not negotiate TLS 1.3 (`TLSv1_3`).
    ProtocolNotTls13,
    /// TLS 1.3 `export_keying_material` failed.
    ExporterUnavailable,
    /// Peer did not present an X.509v3 certificate during TLS 1.3 mTLS.
    MissingPeerCertificate,
    /// Peer X.509v3 certificate failed verification.
    CertificateInvalid(CertVerifyError),
    /// Dialed `SocketAddr` resolved to a different `CellIdentity` than requested
    /// (`[DIST-NAME-001]` DHCP/IP reuse protection).
    TargetCellIdentityMismatch {
        /// Expected nominal Cell identity.
        expected: CellIdentity,
        /// Actual Cell identity authenticated on the peer.
        actual: CellIdentity,
    },
    /// Local verification rejected the peer's handshake claims or signature.
    RejectedByLocal(HandshakeRejectCode),
    /// Remote peer explicitly rejected our handshake with a reason code.
    RejectedByPeer(HandshakeRejectCode),
    /// Peer identity / custody authority verification failed.
    PeerIdentity(PeerIdentityError),
    /// Handshake exceeded configured deadline.
    Timeout,
    /// Malformed handshake wire frame.
    MalformedWire(&'static str),
    /// Socket/TLS I/O error during handshake (`BeforeFirstPayloadByte`).
    Io(io::Error),
}

impl fmt::Display for FenceHandshakeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProtocolNotTls13 => {
                formatter.write_str("TLS 1.3 is mandatory; non-TLS1.3 rejected")
            }
            Self::ExporterUnavailable => formatter.write_str("TLS 1.3 exporter derivation failed"),
            Self::MissingPeerCertificate => formatter.write_str("missing peer mTLS certificate"),
            Self::CertificateInvalid(err) => write!(formatter, "invalid peer certificate: {err}"),
            Self::TargetCellIdentityMismatch { expected, actual } => write!(
                formatter,
                "target CellIdentity mismatch on dialed hint: expected {expected:?}, got {actual:?}"
            ),
            Self::RejectedByLocal(code) => {
                write!(
                    formatter,
                    "local fence handshake gate rejected peer: {code:?}"
                )
            }
            Self::RejectedByPeer(code) => {
                write!(formatter, "remote peer rejected fence handshake: {code:?}")
            }
            Self::PeerIdentity(err) => write!(formatter, "peer identity error: {err}"),
            Self::Timeout => formatter.write_str("5-axis fence handshake timed out"),
            Self::MalformedWire(reason) => write!(formatter, "malformed handshake frame: {reason}"),
            Self::Io(err) => write!(formatter, "handshake I/O error: {err}"),
        }
    }
}

impl Error for FenceHandshakeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CertificateInvalid(err) => Some(err),
            Self::PeerIdentity(err) => Some(err),
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

/// Bundled dependencies for executing client or server 5-axis handshakes.
pub struct HandshakeContext<'a> {
    /// Local 5-axis claims to present.
    pub local_claims: PeerFenceClaims,
    /// Sealed signer for local `PrincipalId`.
    pub local_signer: &'a dyn PeerDigestSigner,
    /// Peer `PrincipalId` & custody tombstone verifier.
    pub peer_verifier: &'a dyn IdentityPeerVerifier,
    /// Monotonic peer `(node_boot_generation, epoch, token)` policy gate.
    pub fence_policy: &'a dyn PeerFencePolicy,
    /// Nonce anti-replay registry.
    pub nonce_registry: &'a FenceNonceRegistry,
    /// Logical/authority timestamp (`ms`) for signing and key-window checks.
    pub now_ms: u64,
    /// Overall handshake timeout.
    pub handshake_timeout: Duration,
}

/// Runs the client (initiator) side of the 4-step mutual 5-axis TLS 1.3
/// Exporter handshake on `stream`.
///
/// # Errors
///
/// Fails closed before any RPC/bulk frame can be sent if TLS 1.3 was not used,
/// if X.509 cert principal mismatches 5-axis claims, if `expected_target_cell`
/// mismatches, if peer boot generation/epoch is stale, or if the TLS Exporter
/// Ed25519 signature fails verification.
#[allow(clippy::missing_panics_doc, clippy::too_many_lines)]
pub async fn run_client_fence_handshake<S>(
    stream: &mut S,
    tls_material: &ExtractedTls13Material,
    ctx: &HandshakeContext<'_>,
    expected_target_cell: Option<CellIdentity>,
    expected_target_principal: Option<PrincipalId>,
) -> Result<AuthenticatedPeerSession, FenceHandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let exporter = tls_material.exporter;
    let verified_cert = verify_node_x509_cert_der(
        tls_material.peer_cert_der.as_ref(),
        ctx.peer_verifier,
        ctx.now_ms,
    )
    .map_err(FenceHandshakeError::CertificateInvalid)?;

    let inner = async {
        // Step 1: Send ClientHelloFence(client_nonce, local_claims)
        let client_nonce = ctx
            .nonce_registry
            .issue_fresh_nonce(ctx.local_claims.cell_identity, &exporter);
        let mut hello = Vec::with_capacity(1 + 32 + 80);
        hello.push(TAG_CLIENT_HELLO);
        hello.extend_from_slice(&client_nonce);
        ctx.local_claims.encode_into(&mut hello);
        write_handshake_frame(stream, &hello).await?;

        // Step 2: Receive ServerChallengeAttestation(server_nonce, server_claims, server_sig)
        let server_msg = read_handshake_frame(stream).await?;
        let (&tag, body) = server_msg
            .split_first()
            .ok_or(FenceHandshakeError::MalformedWire("empty server frame"))?;
        if tag == TAG_HANDSHAKE_REJECT {
            let code = body.first().copied().map_or(
                HandshakeRejectCode::ExporterSignatureInvalid,
                HandshakeRejectCode::from_u8,
            );
            return Err(FenceHandshakeError::RejectedByPeer(code));
        }
        if tag != TAG_SERVER_CHALLENGE_ATTEST || body.len() != 32 + 80 + 64 {
            return Err(FenceHandshakeError::MalformedWire(
                "invalid ServerChallengeAttestation",
            ));
        }
        let server_nonce: [u8; 32] = body[0..32].try_into().expect("32B");
        let server_claims = PeerFenceClaims::decode_exact(&body[32..112])?;
        let server_sig: [u8; 64] = body[112..176].try_into().expect("64B");

        // Enforce nominal CellIdentity match (`[DIST-NAME-001]`), Principal binding, & non-self collision
        if server_claims.cell_identity == ctx.local_claims.cell_identity {
            let _ = send_reject_frame(stream, HandshakeRejectCode::SelfIdentityCollision).await;
            return Err(FenceHandshakeError::RejectedByLocal(
                HandshakeRejectCode::SelfIdentityCollision,
            ));
        }
        if let Some(expected_cell) = expected_target_cell
            && server_claims.cell_identity != expected_cell
        {
            let _ =
                send_reject_frame(stream, HandshakeRejectCode::CertificatePrincipalMismatch).await;
            return Err(FenceHandshakeError::TargetCellIdentityMismatch {
                expected: expected_cell,
                actual: server_claims.cell_identity,
            });
        }
        if server_claims.principal_id != verified_cert.claims.principal_id {
            let _ =
                send_reject_frame(stream, HandshakeRejectCode::CertificatePrincipalMismatch).await;
            return Err(FenceHandshakeError::RejectedByLocal(
                HandshakeRejectCode::CertificatePrincipalMismatch,
            ));
        }
        if let Some(expected_principal) = expected_target_principal
            && server_claims.principal_id != expected_principal
        {
            let _ =
                send_reject_frame(stream, HandshakeRejectCode::CellPrincipalBindingMismatch).await;
            return Err(FenceHandshakeError::RejectedByLocal(
                HandshakeRejectCode::CellPrincipalBindingMismatch,
            ));
        }
        // Read-only pre-check BEFORE signature verification (never mutates watermarks!)
        if let Err(code) = ctx.fence_policy.check_peer_fence(&server_claims) {
            let _ = send_reject_frame(stream, code).await;
            return Err(FenceHandshakeError::RejectedByLocal(code));
        }

        // Verify server's 5-axis TLS Exporter signature
        let expected_server_digest = compute_fence_exporter_digest(
            HandshakeRole::Server,
            &exporter,
            &client_nonce,
            &server_nonce,
            &server_claims,
            ctx.local_claims.cell_identity,
        );
        let verified_peer = ctx
            .peer_verifier
            .verify_peer_digest(
                server_claims.principal_id,
                server_claims.key_id,
                server_claims.key_generation,
                expected_server_digest,
                server_sig,
                ctx.now_ms,
            )
            .map_err(|err| {
                let code = map_identity_err_to_reject_code(&err);
                (err, code)
            });
        let verified_peer = match verified_peer {
            Ok(v) => v,
            Err((err, code)) => {
                let _ = send_reject_frame(stream, code).await;
                return Err(FenceHandshakeError::PeerIdentity(err));
            }
        };

        // Commit monotonic fence watermark ONLY AFTER server's Exporter signature verified!
        if let Err(code) = ctx.fence_policy.commit_verified_peer_fence(&server_claims) {
            let _ = send_reject_frame(stream, code).await;
            return Err(FenceHandshakeError::RejectedByLocal(code));
        }

        // Step 3: Sign & send ClientFenceAttestation(client_sig)
        ctx.nonce_registry.consume_once(&client_nonce)?;
        let client_digest = compute_fence_exporter_digest(
            HandshakeRole::Client,
            &exporter,
            &client_nonce,
            &server_nonce,
            &ctx.local_claims,
            server_claims.cell_identity,
        );
        let client_sig = ctx
            .local_signer
            .sign_domain_digest(CELL_FENCE_HANDSHAKE_DOMAIN, client_digest, ctx.now_ms)
            .map_err(FenceHandshakeError::PeerIdentity)?;
        let mut attest = Vec::with_capacity(1 + 64);
        attest.push(TAG_CLIENT_ATTEST);
        attest.extend_from_slice(&client_sig);
        write_handshake_frame(stream, &attest).await?;

        // Step 4: Receive ServerReadyAck
        let ack = read_handshake_frame(stream).await?;
        let (&ack_tag, ack_body) = ack
            .split_first()
            .ok_or(FenceHandshakeError::MalformedWire("empty server ack"))?;
        if ack_tag == TAG_HANDSHAKE_REJECT {
            let code = ack_body.first().copied().map_or(
                HandshakeRejectCode::ExporterSignatureInvalid,
                HandshakeRejectCode::from_u8,
            );
            return Err(FenceHandshakeError::RejectedByPeer(code));
        }
        if ack_tag != TAG_SERVER_READY_ACK || ack_body.len() != 32 {
            return Err(FenceHandshakeError::MalformedWire("invalid ServerReadyAck"));
        }
        let session_transcript_digest = compute_session_transcript_digest(
            &exporter,
            &client_nonce,
            &server_nonce,
            &ctx.local_claims,
            &server_claims,
        );
        if ack_body != session_transcript_digest {
            return Err(FenceHandshakeError::RejectedByLocal(
                HandshakeRejectCode::ExporterSignatureInvalid,
            ));
        }

        Ok(AuthenticatedPeerSession {
            local_claims: ctx.local_claims,
            peer_claims: server_claims,
            peer_control_domain: verified_peer.control_domain_id,
            tls_exporter_binding: exporter,
            session_transcript_digest,
        })
    };

    timeout(ctx.handshake_timeout, inner)
        .await
        .map_err(|_| FenceHandshakeError::Timeout)?
}

/// Runs the server (listener) side of the 4-step mutual 5-axis TLS 1.3 Exporter
/// handshake on `stream`.
///
/// # Errors
///
/// Fails closed and emits an explicit `TAG_HANDSHAKE_REJECT` frame before
/// closing if any certificate, principal, revocation, boot-generation/epoch, or
/// TLS Exporter signature check fails.
#[allow(clippy::missing_panics_doc, clippy::too_many_lines)]
pub async fn run_server_fence_handshake<S>(
    stream: &mut S,
    tls_material: &ExtractedTls13Material,
    ctx: &HandshakeContext<'_>,
) -> Result<AuthenticatedPeerSession, FenceHandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let exporter = tls_material.exporter;
    let verified_cert = verify_node_x509_cert_der(
        tls_material.peer_cert_der.as_ref(),
        ctx.peer_verifier,
        ctx.now_ms,
    )
    .map_err(FenceHandshakeError::CertificateInvalid)?;

    let inner = async {
        // Step 1: Read ClientHelloFence(client_nonce, client_claims)
        let hello = read_handshake_frame(stream).await?;
        let (&tag, body) = hello
            .split_first()
            .ok_or(FenceHandshakeError::MalformedWire("empty client hello"))?;
        if tag != TAG_CLIENT_HELLO || body.len() != 32 + 80 {
            return Err(FenceHandshakeError::MalformedWire(
                "invalid ClientHelloFence",
            ));
        }
        let client_nonce: [u8; 32] = body[0..32].try_into().expect("32B");
        let client_claims = PeerFenceClaims::decode_exact(&body[32..112])?;

        if client_claims.cell_identity == ctx.local_claims.cell_identity {
            let _ = send_reject_frame(stream, HandshakeRejectCode::SelfIdentityCollision).await;
            return Err(FenceHandshakeError::RejectedByLocal(
                HandshakeRejectCode::SelfIdentityCollision,
            ));
        }
        if client_claims.principal_id != verified_cert.claims.principal_id {
            let _ =
                send_reject_frame(stream, HandshakeRejectCode::CertificatePrincipalMismatch).await;
            return Err(FenceHandshakeError::RejectedByLocal(
                HandshakeRejectCode::CertificatePrincipalMismatch,
            ));
        }
        // Read-only pre-check BEFORE Step 2/3 signature verification (never mutates watermarks!)
        if let Err(code) = ctx.fence_policy.check_peer_fence(&client_claims) {
            let _ = send_reject_frame(stream, code).await;
            return Err(FenceHandshakeError::RejectedByLocal(code));
        }

        // Step 2: Issue server_nonce mixing BOTH TLS 1.3 Exporter secret & client_nonce,
        // sign 5-axis TLS Exporter digest, send ServerChallengeAttestation
        let mut nonce_salt = [0u8; TLS_EXPORTER_LEN + 32];
        nonce_salt[..TLS_EXPORTER_LEN].copy_from_slice(&exporter);
        nonce_salt[TLS_EXPORTER_LEN..].copy_from_slice(&client_nonce);
        let server_nonce = ctx
            .nonce_registry
            .issue_fresh_nonce(ctx.local_claims.cell_identity, &nonce_salt);
        let server_digest = compute_fence_exporter_digest(
            HandshakeRole::Server,
            &exporter,
            &client_nonce,
            &server_nonce,
            &ctx.local_claims,
            client_claims.cell_identity,
        );
        let server_sig = ctx
            .local_signer
            .sign_domain_digest(CELL_FENCE_HANDSHAKE_DOMAIN, server_digest, ctx.now_ms)
            .map_err(FenceHandshakeError::PeerIdentity)?;

        let mut challenge = Vec::with_capacity(1 + 32 + 80 + 64);
        challenge.push(TAG_SERVER_CHALLENGE_ATTEST);
        challenge.extend_from_slice(&server_nonce);
        ctx.local_claims.encode_into(&mut challenge);
        challenge.extend_from_slice(&server_sig);
        write_handshake_frame(stream, &challenge).await?;

        // Step 3: Read ClientFenceAttestation(client_sig) & verify
        let client_attest = read_handshake_frame(stream).await?;
        let (&c_tag, c_body) = client_attest
            .split_first()
            .ok_or(FenceHandshakeError::MalformedWire("empty client attest"))?;
        if c_tag == TAG_HANDSHAKE_REJECT {
            let code = c_body.first().copied().map_or(
                HandshakeRejectCode::ExporterSignatureInvalid,
                HandshakeRejectCode::from_u8,
            );
            return Err(FenceHandshakeError::RejectedByPeer(code));
        }
        if c_tag != TAG_CLIENT_ATTEST || c_body.len() != 64 {
            let _ = send_reject_frame(stream, HandshakeRejectCode::ExporterSignatureInvalid).await;
            return Err(FenceHandshakeError::MalformedWire(
                "invalid ClientFenceAttestation",
            ));
        }
        let client_sig: [u8; 64] = c_body.try_into().expect("64B");

        ctx.nonce_registry.consume_once(&server_nonce)?;
        let expected_client_digest = compute_fence_exporter_digest(
            HandshakeRole::Client,
            &exporter,
            &client_nonce,
            &server_nonce,
            &client_claims,
            ctx.local_claims.cell_identity,
        );
        let verified_peer = match ctx.peer_verifier.verify_peer_digest(
            client_claims.principal_id,
            client_claims.key_id,
            client_claims.key_generation,
            expected_client_digest,
            client_sig,
            ctx.now_ms,
        ) {
            Ok(v) => v,
            Err(err) => {
                let code = map_identity_err_to_reject_code(&err);
                let _ = send_reject_frame(stream, code).await;
                return Err(FenceHandshakeError::PeerIdentity(err));
            }
        };

        // Commit monotonic fence watermark ONLY AFTER client's Exporter signature verified!
        if let Err(code) = ctx.fence_policy.commit_verified_peer_fence(&client_claims) {
            let _ = send_reject_frame(stream, code).await;
            return Err(FenceHandshakeError::RejectedByLocal(code));
        }

        // Step 4: Send ServerReadyAck(session_transcript_digest)
        let session_transcript_digest = compute_session_transcript_digest(
            &exporter,
            &client_nonce,
            &server_nonce,
            &client_claims,
            &ctx.local_claims,
        );
        let mut ack = Vec::with_capacity(1 + 32);
        ack.push(TAG_SERVER_READY_ACK);
        ack.extend_from_slice(&session_transcript_digest);
        write_handshake_frame(stream, &ack).await?;

        Ok(AuthenticatedPeerSession {
            local_claims: ctx.local_claims,
            peer_claims: client_claims,
            peer_control_domain: verified_peer.control_domain_id,
            tls_exporter_binding: exporter,
            session_transcript_digest,
        })
    };

    timeout(ctx.handshake_timeout, inner)
        .await
        .map_err(|_| FenceHandshakeError::Timeout)?
}

fn first_peer_cert(
    tls_state: &CommonState,
) -> Result<&CertificateDer<'static>, FenceHandshakeError> {
    tls_state
        .peer_certificates()
        .and_then(|certs| certs.first())
        .ok_or(FenceHandshakeError::MissingPeerCertificate)
}

fn compute_session_transcript_digest(
    exporter: &[u8; TLS_EXPORTER_LEN],
    client_nonce: &[u8; 32],
    server_nonce: &[u8; 32],
    client_claims: &PeerFenceClaims,
    server_claims: &PeerFenceClaims,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/net/cell-session-transcript/v1");
    hasher.update(exporter);
    hasher.update(client_nonce);
    hasher.update(server_nonce);
    let mut buf = Vec::with_capacity(160);
    client_claims.encode_into(&mut buf);
    server_claims.encode_into(&mut buf);
    hasher.update(&buf);
    hasher.finalize().into()
}

const fn map_identity_err_to_reject_code(err: &PeerIdentityError) -> HandshakeRejectCode {
    match err {
        PeerIdentityError::PrincipalUnknown(_) => HandshakeRejectCode::PrincipalUnknown,
        PeerIdentityError::KeyRevoked { .. }
        | PeerIdentityError::KeyCompromised { .. }
        | PeerIdentityError::KeyRotatedOrStale { .. }
        | PeerIdentityError::KeyNotYetValid
        | PeerIdentityError::KeyExpired => HandshakeRejectCode::KeyRevokedOrCompromised,
        PeerIdentityError::SignatureInvalid
        | PeerIdentityError::CustodySignFailed(_)
        | PeerIdentityError::AuthorityFailure(_) => HandshakeRejectCode::ExporterSignatureInvalid,
    }
}

async fn send_reject_frame<S>(
    stream: &mut S,
    code: HandshakeRejectCode,
) -> Result<(), FenceHandshakeError>
where
    S: AsyncWrite + Unpin,
{
    write_handshake_frame(stream, &[TAG_HANDSHAKE_REJECT, code as u8]).await
}

/// Writes a length-prefixed handshake frame (`<= 1024 B`) onto `stream`.
///
/// # Errors
///
/// Returns [`FenceHandshakeError::MalformedWire`] or [`FenceHandshakeError::Io`].
pub async fn write_handshake_frame<S>(
    stream: &mut S,
    body: &[u8],
) -> Result<(), FenceHandshakeError>
where
    S: AsyncWrite + Unpin,
{
    let len = u32::try_from(body.len())
        .map_err(|_| FenceHandshakeError::MalformedWire("handshake frame > u32"))?;
    stream
        .write_all(&len.to_be_bytes())
        .await
        .map_err(FenceHandshakeError::Io)?;
    stream
        .write_all(body)
        .await
        .map_err(FenceHandshakeError::Io)?;
    stream.flush().await.map_err(FenceHandshakeError::Io)
}

/// Reads a length-prefixed handshake frame (`1..=1024 B`) from `stream`.
///
/// # Errors
///
/// Returns [`FenceHandshakeError::MalformedWire`] or [`FenceHandshakeError::Io`].
pub async fn read_handshake_frame<S>(stream: &mut S) -> Result<Vec<u8>, FenceHandshakeError>
where
    S: AsyncRead + Unpin,
{
    let mut prefix = [0u8; 4];
    stream
        .read_exact(&mut prefix)
        .await
        .map_err(FenceHandshakeError::Io)?;
    let len = u32::from_be_bytes(prefix) as usize;
    if len == 0 || len > HANDSHAKE_MAX_FRAME_BYTES {
        return Err(FenceHandshakeError::MalformedWire(
            "handshake frame length out of bounds",
        ));
    }
    let mut body = vec![0u8; len];
    stream
        .read_exact(&mut body)
        .await
        .map_err(FenceHandshakeError::Io)?;
    Ok(body)
}
