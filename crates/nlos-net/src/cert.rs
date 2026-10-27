//! Pure-Rust RFC 5280 / RFC 8410 Ed25519 X.509v3 node certificate codec and
//! TLS 1.3-only mutual certificate verifier anchored to [`IdentityPeerVerifier`]
//! (`[TCB-ENDPOINT-001]`, `ADR-0026` §2.2 & §6.2).
//!
//! Zero OpenSSL, zero CMake/NASM (`aws-lc-rs`), and zero `rcgen`/`yasna`
//! supply-chain bloat: ephemeral per-boot Ed25519 TLS transport keys are bound
//! to the node's long-term `PrincipalId` via a custody-signed `NLOSCERT` X.509v3
//! extension so private Principal seeds never leave `SigningKeyCustodyVault`.

use std::fmt;
use std::num::NonZeroU64;
use std::sync::Arc;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use nlos_types::{ControlDomainId, Generation, KeyId, PrincipalId};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, DistinguishedName, Error as TlsError,
    PeerIncompatible, ServerConfig, SignatureScheme,
};
use sha2::{Digest, Sha256};

use crate::identity::{IdentityPeerVerifier, PeerDigestSigner, PeerIdentityError, VerifiedPeerKey};

/// Domain separator for custody signatures binding an ephemeral TLS 1.3
/// Ed25519 public key to `(PrincipalId, KeyId, key_generation)`.
pub const TLS_CERT_PRINCIPAL_BINDING_DOMAIN: &[u8] = b"llmos/net/x509-principal-binding/v1";

const NLOS_CERT_MAGIC: &[u8; 8] = b"NLOSCERT";
const NLOS_CERT_EXTENSION_PAYLOAD_LEN: usize = 128;

/// `1.3.101.112` (`id-Ed25519`, RFC 8410).
const OID_ED25519: &[u8] = &[0x2b, 0x65, 0x70];
/// `2.5.4.3` (`id-at-commonName`).
const OID_COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
/// Private enterprise NLOS X.509v3 Principal-binding extension OID
/// (`1.3.6.1.4.1.59999.26.1`).
const OID_NLOS_PRINCIPAL_BINDING: &[u8] =
    &[0x2b, 0x06, 0x01, 0x04, 0x01, 0x83, 0xd4, 0x5f, 0x1a, 0x01];

/// Fixed SNI server name used on cross-Cell mTLS connections (`IP:port` and DNS
/// hostnames are never treated as identity per `[DIST-NAME-001]`).
pub const NLOS_CELL_TLS_SNI: &str = "cell.nlos.internal";

/// Structured claims embedded in a node's X.509v3 `NLOSCERT` extension.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeIdentityCertClaims {
    /// Node `PrincipalId`.
    pub principal_id: PrincipalId,
    /// Custody `KeyId` that signed this TLS transport key binding.
    pub key_id: KeyId,
    /// Custody key generation (`rotation_epoch`) active at issuance.
    pub key_generation: Generation,
    /// Validity start timestamp (`ms`).
    pub issued_at_ms: u64,
    /// Validity end timestamp (`ms`).
    pub expires_at_ms: u64,
    /// Ephemeral per-boot Ed25519 TLS 1.3 public key in `SubjectPublicKeyInfo`.
    pub tls_public_key: [u8; 32],
}

impl NodeIdentityCertClaims {
    /// Computes the canonical 32-byte digest signed inside
    /// `SigningKeyCustodyVault` to authorize `tls_public_key`.
    #[must_use]
    pub fn binding_digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(TLS_CERT_PRINCIPAL_BINDING_DOMAIN);
        hasher.update(self.principal_id.as_bytes());
        hasher.update(self.key_id.as_bytes());
        hasher.update(self.key_generation.get().to_be_bytes());
        hasher.update(self.issued_at_ms.to_be_bytes());
        hasher.update(self.expires_at_ms.to_be_bytes());
        hasher.update(self.tls_public_key);
        hasher.finalize().into()
    }
}

/// Verified node X.509v3 certificate metadata anchored in `IdentityAuthority`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerifiedNodeCert {
    /// Parsed certificate binding claims.
    pub claims: NodeIdentityCertClaims,
    /// Authoritative `ControlDomainId` of `claims.principal_id`.
    pub control_domain_id: ControlDomainId,
}

/// Issued TLS 1.3 node credentials ready to build [`ServerConfig`] and
/// [`ClientConfig`].
pub struct NodeTlsCredentials {
    cert_der: CertificateDer<'static>,
    pkcs8_v2_der: Vec<u8>,
    claims: NodeIdentityCertClaims,
}

impl fmt::Debug for NodeTlsCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NodeTlsCredentials")
            .field("claims", &self.claims)
            .field("cert_der_len", &self.cert_der.as_ref().len())
            .field("pkcs8_v2_der", &"<redacted-ed25519-pkcs8>")
            .finish()
    }
}

impl Clone for NodeTlsCredentials {
    fn clone(&self) -> Self {
        Self {
            cert_der: self.cert_der.clone(),
            pkcs8_v2_der: self.pkcs8_v2_der.clone(),
            claims: self.claims,
        }
    }
}

impl NodeTlsCredentials {
    /// Issues an ephemeral Ed25519 TLS 1.3 X.509v3 node certificate bound to
    /// `signer.principal_id()` via a sealed custody signature.
    ///
    /// # Errors
    ///
    /// Fails closed if `expires_at_ms <= issued_at_ms` or if custody signing
    /// fails.
    pub fn issue(
        signer: &dyn PeerDigestSigner,
        tls_transport_seed: [u8; 32],
        issued_at_ms: u64,
        expires_at_ms: u64,
    ) -> Result<Self, CertVerifyError> {
        if expires_at_ms <= issued_at_ms {
            return Err(CertVerifyError::InvalidValidityWindow);
        }
        let tls_signing_key = SigningKey::from_bytes(&tls_transport_seed);
        let tls_public_key = tls_signing_key.verifying_key().to_bytes();

        let claims = NodeIdentityCertClaims {
            principal_id: signer.principal_id(),
            key_id: signer.key_id(),
            key_generation: signer.key_generation(),
            issued_at_ms,
            expires_at_ms,
            tls_public_key,
        };
        let custody_sig = signer
            .sign_domain_digest(
                TLS_CERT_PRINCIPAL_BINDING_DOMAIN,
                claims.binding_digest(),
                issued_at_ms,
            )
            .map_err(CertVerifyError::PeerIdentity)?;

        let cert_bytes = build_x509v3_ed25519_cert_der(&tls_signing_key, &claims, &custody_sig);
        let pkcs8_v2_der = build_pkcs8_v2_ed25519_der(&tls_transport_seed, &tls_public_key);

        Ok(Self {
            cert_der: CertificateDer::from(cert_bytes),
            pkcs8_v2_der,
            claims,
        })
    }

    /// Public X.509v3 DER certificate.
    #[must_use]
    pub fn certificate_der(&self) -> CertificateDer<'static> {
        self.cert_der.clone()
    }

    /// Cloned `PrivateKeyDer` for `rustls` configuration.
    #[must_use]
    pub fn private_key_der(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.pkcs8_v2_der.clone()))
    }

    /// Claims bound inside this certificate.
    #[must_use]
    pub const fn claims(&self) -> NodeIdentityCertClaims {
        self.claims
    }
}

/// Fail-closed X.509v3 node certificate verification errors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CertVerifyError {
    /// Certificate validity bounds were inverted (`expires_at_ms <= issued_at_ms`).
    InvalidValidityWindow,
    /// Malformed ASN.1 DER structure or unexpected tag/length.
    MalformedDer(&'static str),
    /// Certificate uses a non-Ed25519 signature algorithm.
    UnsupportedAlgorithm,
    /// Missing or malformed `NLOSCERT` Principal-binding extension.
    MissingPrincipalBindingExtension,
    /// Certificate timestamp is not yet valid (`verified_at_ms < issued_at_ms`).
    CertNotYetValid,
    /// Certificate timestamp has expired (`verified_at_ms > expires_at_ms`).
    CertExpired,
    /// Self-signature over `tbsCertificate` failed strict Ed25519 verification.
    InvalidCertSelfSignature,
    /// Principal identity or custody verification failed closed.
    PeerIdentity(PeerIdentityError),
}

impl fmt::Display for CertVerifyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidValidityWindow => {
                formatter.write_str("invalid X.509 certificate validity window")
            }
            Self::MalformedDer(detail) => write!(formatter, "malformed X.509 DER: {detail}"),
            Self::UnsupportedAlgorithm => {
                formatter.write_str("unsupported X.509 algorithm (Ed25519 required)")
            }
            Self::MissingPrincipalBindingExtension => {
                formatter.write_str("missing NLOSCERT Principal-binding X.509v3 extension")
            }
            Self::CertNotYetValid => formatter.write_str("node TLS certificate is not yet valid"),
            Self::CertExpired => formatter.write_str("node TLS certificate has expired"),
            Self::InvalidCertSelfSignature => {
                formatter.write_str("X.509 tbsCertificate Ed25519 signature is invalid")
            }
            Self::PeerIdentity(err) => write!(formatter, "peer identity rejected: {err}"),
        }
    }
}

impl std::error::Error for CertVerifyError {}

/// Parses `cert_der` under strict canonical ASN.1 DER rules, verifies its
/// Ed25519 self-signature, and verifies the `NLOSCERT` Principal binding
/// against `peer_verifier` at `verified_at_ms`.
///
/// # Errors
///
/// Fails closed on any DER malformation, expired/not-yet-valid window, unknown
/// `PrincipalId`, revoked/compromised custody key, or invalid signature.
#[allow(clippy::missing_panics_doc, clippy::too_many_lines)]
pub fn verify_node_x509_cert_der(
    cert_der: &[u8],
    peer_verifier: &dyn IdentityPeerVerifier,
    verified_at_ms: u64,
) -> Result<VerifiedNodeCert, CertVerifyError> {
    let mut outer = DerReader::new(cert_der);
    let mut cert_seq = outer.read_tlv(0x30, "Certificate SEQUENCE")?;
    if !outer.is_empty() {
        return Err(CertVerifyError::MalformedDer(
            "trailing bytes after Certificate",
        ));
    }

    let (tbs_raw, mut tbs_body) = cert_seq.read_tlv_with_raw(0x30, "TBSCertificate")?;
    let mut outer_sig_alg = cert_seq.read_tlv(0x30, "signatureAlgorithm")?;
    expect_ed25519_alg(&mut outer_sig_alg)?;
    let sig_bits = cert_seq.read_tlv_bytes(0x03, "signatureValue BIT STRING")?;
    if !cert_seq.is_empty() {
        return Err(CertVerifyError::MalformedDer(
            "trailing fields in Certificate",
        ));
    }
    if sig_bits.len() != 65 || sig_bits[0] != 0x00 {
        return Err(CertVerifyError::MalformedDer(
            "invalid Ed25519 signatureValue length",
        ));
    }
    let cert_sig_bytes: [u8; 64] = sig_bits[1..]
        .try_into()
        .map_err(|_| CertVerifyError::MalformedDer("signature slice"))?;

    // Parse TBSCertificate fields in exact RFC 5280 order:
    // version [0], serialNumber, signature, issuer, validity, subject, subjectPublicKeyInfo, extensions [3].
    let version_explicit = tbs_body.read_tlv_bytes(0xa0, "TBS version [0]")?;
    if version_explicit != [0x02, 0x01, 0x02] {
        return Err(CertVerifyError::MalformedDer(
            "X.509 version must be v3 (2)",
        ));
    }
    let serial = tbs_body.read_tlv_bytes(0x02, "serialNumber")?;
    if serial.is_empty() || serial.len() > 20 || (serial[0] & 0x80) != 0 {
        return Err(CertVerifyError::MalformedDer("invalid serialNumber"));
    }
    let mut inner_sig_alg = tbs_body.read_tlv(0x30, "TBS signature alg")?;
    expect_ed25519_alg(&mut inner_sig_alg)?;
    let _issuer = tbs_body.read_tlv_bytes(0x30, "issuer")?;
    let _validity = tbs_body.read_tlv_bytes(0x30, "validity")?;
    let _subject = tbs_body.read_tlv_bytes(0x30, "subject")?;

    let mut spki = tbs_body.read_tlv(0x30, "subjectPublicKeyInfo")?;
    let mut spki_alg = spki.read_tlv(0x30, "SPKI algorithm")?;
    expect_ed25519_alg(&mut spki_alg)?;
    let spki_bits = spki.read_tlv_bytes(0x03, "SPKI BIT STRING")?;
    if !spki.is_empty() || spki_bits.len() != 33 || spki_bits[0] != 0x00 {
        return Err(CertVerifyError::MalformedDer("invalid Ed25519 SPKI key"));
    }
    let tls_public_key: [u8; 32] = spki_bits[1..]
        .try_into()
        .map_err(|_| CertVerifyError::MalformedDer("SPKI pubkey slice"))?;

    let mut ext_wrapper = tbs_body.read_tlv(0xa3, "extensions [3]")?;
    if !tbs_body.is_empty() {
        return Err(CertVerifyError::MalformedDer(
            "trailing fields in TBSCertificate",
        ));
    }
    let mut ext_seq = ext_wrapper.read_tlv(0x30, "Extensions SEQUENCE")?;
    if !ext_wrapper.is_empty() {
        return Err(CertVerifyError::MalformedDer(
            "trailing bytes in extensions [3]",
        ));
    }

    let mut found_payload: Option<[u8; NLOS_CERT_EXTENSION_PAYLOAD_LEN]> = None;
    while !ext_seq.is_empty() {
        let mut ext = ext_seq.read_tlv(0x30, "Extension")?;
        let oid = ext.read_tlv_bytes(0x06, "extnID OID")?;
        // Optional critical BOOLEAN (tag 0x01)
        if ext.peek_tag() == Some(0x01) {
            let _critical = ext.read_tlv_bytes(0x01, "critical BOOLEAN")?;
        }
        let val = ext.read_tlv_bytes(0x04, "extnValue OCTET STRING")?;
        if !ext.is_empty() {
            return Err(CertVerifyError::MalformedDer("trailing bytes in Extension"));
        }
        if oid == OID_NLOS_PRINCIPAL_BINDING {
            if found_payload.is_some() {
                return Err(CertVerifyError::MalformedDer(
                    "duplicate NLOSCERT Principal-binding extension",
                ));
            }
            let payload: [u8; NLOS_CERT_EXTENSION_PAYLOAD_LEN] = val
                .try_into()
                .map_err(|_| CertVerifyError::MissingPrincipalBindingExtension)?;
            found_payload = Some(payload);
        }
    }

    let payload = found_payload.ok_or(CertVerifyError::MissingPrincipalBindingExtension)?;
    if &payload[0..8] != NLOS_CERT_MAGIC {
        return Err(CertVerifyError::MissingPrincipalBindingExtension);
    }

    // 1. Verify self-signature of `tls_public_key` over `tbs_raw`.
    let tls_vk = VerifyingKey::from_bytes(&tls_public_key)
        .map_err(|_| CertVerifyError::InvalidCertSelfSignature)?;
    let cert_sig = Signature::from_bytes(&cert_sig_bytes);
    tls_vk
        .verify_strict(tbs_raw, &cert_sig)
        .map_err(|_| CertVerifyError::InvalidCertSelfSignature)?;

    // 2. Decode claims + custody binding signature.
    let principal_bytes: [u8; 16] = payload[8..24].try_into().expect("16B slice");
    let key_bytes: [u8; 16] = payload[24..40].try_into().expect("16B slice");
    let key_gen_raw = u64::from_be_bytes(payload[40..48].try_into().expect("8B slice"));
    let issued_at_ms = u64::from_be_bytes(payload[48..56].try_into().expect("8B slice"));
    let expires_at_ms = u64::from_be_bytes(payload[56..64].try_into().expect("8B slice"));
    let custody_sig: [u8; 64] = payload[64..128].try_into().expect("64B slice");

    let key_generation =
        NonZeroU64::new(key_gen_raw)
            .map(Generation::new)
            .ok_or(CertVerifyError::MalformedDer(
                "zero key_generation in NLOSCERT",
            ))?;
    if expires_at_ms <= issued_at_ms {
        return Err(CertVerifyError::InvalidValidityWindow);
    }
    if verified_at_ms < issued_at_ms {
        return Err(CertVerifyError::CertNotYetValid);
    }
    if verified_at_ms > expires_at_ms {
        return Err(CertVerifyError::CertExpired);
    }

    let claims = NodeIdentityCertClaims {
        principal_id: PrincipalId::from_bytes(principal_bytes),
        key_id: KeyId::from_bytes(key_bytes),
        key_generation,
        issued_at_ms,
        expires_at_ms,
        tls_public_key,
    };

    // 3. Anchor `(principal_id, key_id, key_generation)` and custody signature
    // against IdentityPeerVerifier (IdentityAuthority + SigningKeyCustodyVault).
    let VerifiedPeerKey {
        control_domain_id, ..
    } = peer_verifier
        .verify_peer_digest(
            claims.principal_id,
            claims.key_id,
            claims.key_generation,
            claims.binding_digest(),
            custody_sig,
            verified_at_ms,
        )
        .map_err(CertVerifyError::PeerIdentity)?;

    Ok(VerifiedNodeCert {
        claims,
        control_domain_id,
    })
}

/// Source for the monotonic/logical verification timestamp (`ms`) used when
/// validating Principal key validity intervals during TLS 1.3 mTLS handshakes.
#[derive(Clone)]
pub enum VerificationTimeSource {
    /// Converts `rustls`'s `UnixTime` (`secs * 1000`).
    TlsUnixTime,
    /// Caller-supplied deterministic clock callback (e.g. `AuthorityClock` or
    /// deterministic test tick source).
    CustomMs(Arc<dyn Fn() -> u64 + Send + Sync>),
}

impl fmt::Debug for VerificationTimeSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TlsUnixTime => formatter.write_str("TlsUnixTime"),
            Self::CustomMs(_) => formatter.write_str("CustomMs(<fn>)"),
        }
    }
}

impl VerificationTimeSource {
    fn resolve_ms(&self, now: UnixTime) -> u64 {
        match self {
            Self::TlsUnixTime => now.as_secs().saturating_mul(1000),
            Self::CustomMs(f) => f(),
        }
    }
}

/// Mutual TLS 1.3 certificate verifier enforcing:
/// 1. TLS 1.3 ONLY (`verify_tls12_signature` unconditionally fails closed);
/// 2. `ring` cryptographic verification of TLS 1.3 `CertificateVerify`;
/// 3. Strict `NLOSCERT` X.509v3 Principal binding anchored in
///    [`IdentityPeerVerifier`] (revoked/unknown keys fail closed at TLS
///    handshake!).
pub struct MtlsNodeCertVerifier {
    peer_verifier: Arc<dyn IdentityPeerVerifier>,
    provider: Arc<CryptoProvider>,
    time_source: VerificationTimeSource,
    root_hint_subjects: [DistinguishedName; 0],
}

impl fmt::Debug for MtlsNodeCertVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MtlsNodeCertVerifier")
            .field("time_source", &self.time_source)
            .finish_non_exhaustive()
    }
}

impl MtlsNodeCertVerifier {
    /// Constructs a mutual TLS 1.3 verifier backed by `peer_verifier` and the
    /// `ring` `CryptoProvider`.
    #[must_use]
    pub fn new(
        peer_verifier: Arc<dyn IdentityPeerVerifier>,
        time_source: VerificationTimeSource,
    ) -> Self {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        Self {
            peer_verifier,
            provider,
            time_source,
            root_hint_subjects: [],
        }
    }

    fn verify_peer_end_entity(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<VerifiedNodeCert, TlsError> {
        if !intermediates.is_empty() {
            return Err(TlsError::InvalidCertificate(CertificateError::BadEncoding));
        }
        let verified_at_ms = self.time_source.resolve_ms(now);
        verify_node_x509_cert_der(
            end_entity.as_ref(),
            self.peer_verifier.as_ref(),
            verified_at_ms,
        )
        .map_err(|err| map_cert_error_to_tls(&err))
    }
}

impl ServerCertVerifier for MtlsNodeCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        self.verify_peer_end_entity(end_entity, intermediates, now)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::PeerIncompatible(
            PeerIncompatible::Tls12NotOfferedOrEnabled,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

impl ClientCertVerifier for MtlsNodeCertVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &self.root_hint_subjects
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        self.verify_peer_end_entity(end_entity, intermediates, now)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::PeerIncompatible(
            PeerIncompatible::Tls12NotOfferedOrEnabled,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

/// Builds a strict TLS 1.3-only (`enable_early_data = false`, mandatory client
/// mTLS) [`ServerConfig`] using `ring` and [`MtlsNodeCertVerifier`].
///
/// # Errors
///
/// Fails if `rustls` rejects the protocol or key configuration.
pub fn build_tls13_server_config(
    credentials: &NodeTlsCredentials,
    verifier: Arc<MtlsNodeCertVerifier>,
) -> Result<Arc<ServerConfig>, TlsError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![credentials.certificate_der()],
            credentials.private_key_der(),
        )?;
    // Explicitly reject TLS 1.3 0-RTT early data to eliminate replay hazard on
    // mutating channels (`ADR-0026` §3.1).
    config.max_early_data_size = 0;
    config.alpn_protocols = vec![b"nlos-cell/1".to_vec()];
    Ok(Arc::new(config))
}

/// Builds a strict TLS 1.3-only (`enable_early_data = false`, mandatory client
/// mTLS) [`ClientConfig`] using `ring` and [`MtlsNodeCertVerifier`].
///
/// # Errors
///
/// Fails if `rustls` rejects the protocol or key configuration.
pub fn build_tls13_client_config(
    credentials: &NodeTlsCredentials,
    verifier: Arc<MtlsNodeCertVerifier>,
) -> Result<Arc<ClientConfig>, TlsError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(
            vec![credentials.certificate_der()],
            credentials.private_key_der(),
        )?;
    config.enable_early_data = false;
    config.alpn_protocols = vec![b"nlos-cell/1".to_vec()];
    Ok(Arc::new(config))
}

fn map_cert_error_to_tls(err: &CertVerifyError) -> TlsError {
    match err {
        CertVerifyError::CertNotYetValid
        | CertVerifyError::PeerIdentity(PeerIdentityError::KeyNotYetValid) => {
            TlsError::InvalidCertificate(CertificateError::NotValidYet)
        }
        CertVerifyError::CertExpired
        | CertVerifyError::PeerIdentity(PeerIdentityError::KeyExpired) => {
            TlsError::InvalidCertificate(CertificateError::Expired)
        }
        CertVerifyError::PeerIdentity(PeerIdentityError::PrincipalUnknown(_)) => {
            TlsError::InvalidCertificate(CertificateError::UnknownIssuer)
        }
        CertVerifyError::PeerIdentity(
            PeerIdentityError::KeyRevoked { .. }
            | PeerIdentityError::KeyCompromised { .. }
            | PeerIdentityError::KeyRotatedOrStale { .. },
        ) => TlsError::InvalidCertificate(CertificateError::Revoked),
        CertVerifyError::InvalidCertSelfSignature
        | CertVerifyError::PeerIdentity(PeerIdentityError::SignatureInvalid) => {
            TlsError::InvalidCertificate(CertificateError::BadSignature)
        }
        CertVerifyError::InvalidValidityWindow
        | CertVerifyError::MalformedDer(_)
        | CertVerifyError::UnsupportedAlgorithm
        | CertVerifyError::MissingPrincipalBindingExtension
        | CertVerifyError::PeerIdentity(
            PeerIdentityError::CustodySignFailed(_) | PeerIdentityError::AuthorityFailure(_),
        ) => TlsError::InvalidCertificate(CertificateError::BadEncoding),
    }
}

fn build_pkcs8_v2_ed25519_der(seed: &[u8; 32], public_key: &[u8; 32]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(81);
    // version INTEGER 1 (v2 OneAsymmetricKey, RFC 8410 §7)
    inner.extend_from_slice(&[0x02, 0x01, 0x01]);
    // privateKeyAlgorithm SEQUENCE { OID 1.3.101.112 }
    inner.extend_from_slice(&[0x30, 0x05, 0x06, 0x03]);
    inner.extend_from_slice(OID_ED25519);
    // privateKey OCTET STRING { CurvePrivateKey OCTET STRING (32B) }
    inner.extend_from_slice(&[0x04, 0x22, 0x04, 0x20]);
    inner.extend_from_slice(seed);
    // publicKey [1] IMPLICIT BIT STRING (0 unused bits + 32B pubkey)
    inner.extend_from_slice(&[0x81, 0x21, 0x00]);
    inner.extend_from_slice(public_key);
    der_tlv(0x30, &inner)
}

fn build_x509v3_ed25519_cert_der(
    tls_signing_key: &SigningKey,
    claims: &NodeIdentityCertClaims,
    custody_sig: &[u8; 64],
) -> Vec<u8> {
    let mut serial_hash = Sha256::new();
    serial_hash.update(b"llmos/net/x509-serial/v1");
    serial_hash.update(claims.principal_id.as_bytes());
    serial_hash.update(claims.tls_public_key);
    let digest: [u8; 32] = serial_hash.finalize().into();
    let mut serial_bytes = [0u8; 16];
    serial_bytes.copy_from_slice(&digest[..16]);
    serial_bytes[0] = (serial_bytes[0] & 0x7f) | 0x01;

    let alg_ed25519 = der_tlv(0x30, &der_tlv(0x06, OID_ED25519));
    let cn_atv = {
        let mut s = der_tlv(0x06, OID_COMMON_NAME);
        s.extend_from_slice(&der_tlv(0x0c, NLOS_CELL_TLS_SNI.as_bytes()));
        der_tlv(0x30, &s)
    };
    let name_dn = der_tlv(0x30, &der_tlv(0x31, &cn_atv));

    let validity = {
        let mut v = der_tlv(0x18, b"20200101000000Z");
        v.extend_from_slice(&der_tlv(0x18, b"20991231235959Z"));
        der_tlv(0x30, &v)
    };

    let spki = {
        let mut key_bits = Vec::with_capacity(33);
        key_bits.push(0x00);
        key_bits.extend_from_slice(&claims.tls_public_key);
        let mut s = alg_ed25519.clone();
        s.extend_from_slice(&der_tlv(0x03, &key_bits));
        der_tlv(0x30, &s)
    };

    let mut ext_payload = Vec::with_capacity(NLOS_CERT_EXTENSION_PAYLOAD_LEN);
    ext_payload.extend_from_slice(NLOS_CERT_MAGIC);
    ext_payload.extend_from_slice(claims.principal_id.as_bytes());
    ext_payload.extend_from_slice(claims.key_id.as_bytes());
    ext_payload.extend_from_slice(&claims.key_generation.get().to_be_bytes());
    ext_payload.extend_from_slice(&claims.issued_at_ms.to_be_bytes());
    ext_payload.extend_from_slice(&claims.expires_at_ms.to_be_bytes());
    ext_payload.extend_from_slice(custody_sig);

    let nlos_ext = {
        let mut e = der_tlv(0x06, OID_NLOS_PRINCIPAL_BINDING);
        e.extend_from_slice(&der_tlv(0x04, &ext_payload));
        der_tlv(0x30, &e)
    };
    let extensions = der_tlv(0xa3, &der_tlv(0x30, &nlos_ext));

    let mut tbs_inner = Vec::with_capacity(320);
    tbs_inner.extend_from_slice(&[0xa0, 0x03, 0x02, 0x01, 0x02]);
    tbs_inner.extend_from_slice(&der_tlv(0x02, &serial_bytes));
    tbs_inner.extend_from_slice(&alg_ed25519);
    tbs_inner.extend_from_slice(&name_dn);
    tbs_inner.extend_from_slice(&validity);
    tbs_inner.extend_from_slice(&name_dn);
    tbs_inner.extend_from_slice(&spki);
    tbs_inner.extend_from_slice(&extensions);

    let tbs_der = der_tlv(0x30, &tbs_inner);
    let cert_sig = tls_signing_key.sign(&tbs_der).to_bytes();
    let mut sig_bits = Vec::with_capacity(65);
    sig_bits.push(0x00);
    sig_bits.extend_from_slice(&cert_sig);

    let mut cert_inner = Vec::with_capacity(tbs_der.len() + 80);
    cert_inner.extend_from_slice(&tbs_der);
    cert_inner.extend_from_slice(&alg_ed25519);
    cert_inner.extend_from_slice(&der_tlv(0x03, &sig_bits));
    der_tlv(0x30, &cert_inner)
}

fn der_tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let len = content.len();
    let mut out = Vec::with_capacity(4 + len);
    out.push(tag);
    if len < 0x80 {
        out.push(u8::try_from(len).expect("< 128"));
    } else if len <= 0xff {
        out.push(0x81);
        out.push(u8::try_from(len).expect("<= 255"));
    } else {
        let len_u16 = u16::try_from(len).expect("X.509 cert fits in u16");
        out.push(0x82);
        out.extend_from_slice(&len_u16.to_be_bytes());
    }
    out.extend_from_slice(content);
    out
}

fn expect_ed25519_alg(reader: &mut DerReader<'_>) -> Result<(), CertVerifyError> {
    let oid = reader
        .read_tlv_bytes(0x06, "AlgorithmIdentifier OID")
        .map_err(|_| CertVerifyError::UnsupportedAlgorithm)?;
    if oid != OID_ED25519 || !reader.is_empty() {
        return Err(CertVerifyError::UnsupportedAlgorithm);
    }
    Ok(())
}

struct DerReader<'a> {
    bytes: &'a [u8],
}

impl<'a> DerReader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn peek_tag(&self) -> Option<u8> {
        self.bytes.first().copied()
    }

    fn read_tlv(&mut self, expected_tag: u8, label: &'static str) -> Result<Self, CertVerifyError> {
        let (_, body) = self.read_tlv_with_raw(expected_tag, label)?;
        Ok(body)
    }

    fn read_tlv_bytes(
        &mut self,
        expected_tag: u8,
        label: &'static str,
    ) -> Result<&'a [u8], CertVerifyError> {
        let body = self.read_tlv(expected_tag, label)?;
        Ok(body.bytes)
    }

    fn read_tlv_with_raw(
        &mut self,
        expected_tag: u8,
        label: &'static str,
    ) -> Result<(&'a [u8], Self), CertVerifyError> {
        let start = self.bytes;
        let (&tag, rest) = start
            .split_first()
            .ok_or(CertVerifyError::MalformedDer(label))?;
        if tag != expected_tag {
            return Err(CertVerifyError::MalformedDer(label));
        }
        let (&len_head, rest) = rest
            .split_first()
            .ok_or(CertVerifyError::MalformedDer(label))?;
        let (len, after_len, header_len) = if len_head < 0x80 {
            (len_head as usize, rest, 2usize)
        } else if len_head == 0x81 {
            let (&b0, after) = rest
                .split_first()
                .ok_or(CertVerifyError::MalformedDer(label))?;
            if b0 < 0x80 {
                return Err(CertVerifyError::MalformedDer("non-minimal DER length"));
            }
            (b0 as usize, after, 3usize)
        } else if len_head == 0x82 {
            if rest.len() < 2 {
                return Err(CertVerifyError::MalformedDer(label));
            }
            let val = u16::from_be_bytes([rest[0], rest[1]]) as usize;
            if val < 0x100 {
                return Err(CertVerifyError::MalformedDer("non-minimal DER length"));
            }
            (val, &rest[2..], 4usize)
        } else {
            return Err(CertVerifyError::MalformedDer(
                "unsupported DER length octets",
            ));
        };

        if after_len.len() < len {
            return Err(CertVerifyError::MalformedDer(label));
        }
        let content = &after_len[..len];
        self.bytes = &after_len[len..];
        let raw_tlv = &start[..header_len + len];
        Ok((raw_tlv, Self::new(content)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::VerifiedPeerKey;

    struct StaticTestSigner {
        principal: PrincipalId,
        domain: ControlDomainId,
        key_id: KeyId,
        key_gen: Generation,
        signing_key: SigningKey,
    }

    impl StaticTestSigner {
        fn sample() -> Self {
            Self {
                principal: PrincipalId::from_bytes([0x11; 16]),
                domain: ControlDomainId::from_bytes([0x22; 16]),
                key_id: KeyId::from_bytes([0x33; 16]),
                key_gen: Generation::new(NonZeroU64::new(1).expect("nz")),
                signing_key: SigningKey::from_bytes(&[0x44; 32]),
            }
        }
    }

    impl PeerDigestSigner for StaticTestSigner {
        fn principal_id(&self) -> PrincipalId {
            self.principal
        }

        fn key_id(&self) -> KeyId {
            self.key_id
        }

        fn key_generation(&self) -> Generation {
            self.key_gen
        }

        fn sign_domain_digest(
            &self,
            _domain_tag: &'static [u8],
            message_digest: [u8; 32],
            _signed_at_ms: u64,
        ) -> Result<[u8; 64], PeerIdentityError> {
            Ok(self.signing_key.sign(&message_digest).to_bytes())
        }
    }

    impl IdentityPeerVerifier for StaticTestSigner {
        fn verify_peer_digest(
            &self,
            principal: PrincipalId,
            key_id: KeyId,
            key_generation: Generation,
            message_digest: [u8; 32],
            signature: [u8; 64],
            _verified_at_ms: u64,
        ) -> Result<VerifiedPeerKey, PeerIdentityError> {
            if principal != self.principal {
                return Err(PeerIdentityError::PrincipalUnknown(principal));
            }
            let sig = Signature::from_bytes(&signature);
            self.signing_key
                .verifying_key()
                .verify_strict(&message_digest, &sig)
                .map_err(|_| PeerIdentityError::SignatureInvalid)?;
            Ok(VerifiedPeerKey {
                principal_id: self.principal,
                control_domain_id: self.domain,
                key_id,
                key_generation,
            })
        }
    }

    #[test]
    fn x509_ed25519_cert_roundtrips_and_rejects_expiry_or_tampering() {
        let signer = StaticTestSigner::sample();
        let creds =
            NodeTlsCredentials::issue(&signer, [0x55; 32], 1_000, 10_000).expect("issue node cert");
        let der = creds.certificate_der();

        let verified =
            verify_node_x509_cert_der(der.as_ref(), &signer, 5_000).expect("verify within window");
        assert_eq!(verified.claims.principal_id, signer.principal);
        assert_eq!(verified.control_domain_id, signer.domain);

        assert_eq!(
            verify_node_x509_cert_der(der.as_ref(), &signer, 999),
            Err(CertVerifyError::CertNotYetValid)
        );
        assert_eq!(
            verify_node_x509_cert_der(der.as_ref(), &signer, 10_001),
            Err(CertVerifyError::CertExpired)
        );

        let mut tampered = der.as_ref().to_vec();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert_eq!(
            verify_node_x509_cert_der(&tampered, &signer, 5_000),
            Err(CertVerifyError::InvalidCertSelfSignature)
        );

        // RFC 5280 §4.2: duplicate `OID_NLOS_PRINCIPAL_BINDING` extensions MUST fail closed
        // even when properly re-signed by the TLS leaf private key!
        let tls_sk = SigningKey::from_bytes(&[0x55; 32]);
        let claims = NodeIdentityCertClaims {
            principal_id: signer.principal,
            key_id: signer.key_id,
            key_generation: signer.key_gen,
            issued_at_ms: 1_000,
            expires_at_ms: 10_000,
            tls_public_key: tls_sk.verifying_key().to_bytes(),
        };
        let custody_sig = signer
            .sign_domain_digest(
                TLS_CERT_PRINCIPAL_BINDING_DOMAIN,
                claims.binding_digest(),
                1_000,
            )
            .expect("sign binding");
        let valid_single = build_x509v3_ed25519_cert_der(&tls_sk, &claims, &custody_sig);
        let mut reader = DerReader::new(&valid_single);
        let mut cert_seq = reader.read_tlv(0x30, "Certificate").expect("cert");
        let (_, mut tbs_body) = cert_seq
            .read_tlv_with_raw(0x30, "TBSCertificate")
            .expect("tbs");
        let v_tlv = tbs_body.read_tlv_with_raw(0xa0, "v").expect("v").0;
        let s_tlv = tbs_body.read_tlv_with_raw(0x02, "s").expect("s").0;
        let a_tlv = tbs_body.read_tlv_with_raw(0x30, "a").expect("a").0;
        let i_tlv = tbs_body.read_tlv_with_raw(0x30, "i").expect("i").0;
        let val_tlv = tbs_body.read_tlv_with_raw(0x30, "val").expect("val").0;
        let sub_tlv = tbs_body.read_tlv_with_raw(0x30, "sub").expect("sub").0;
        let spki_tlv = tbs_body.read_tlv_with_raw(0x30, "spki").expect("spki").0;
        let mut ext_wrap = tbs_body.read_tlv(0xa3, "ext").expect("ext");
        let ext_inner = ext_wrap.read_tlv_bytes(0x30, "ext seq").expect("ext seq");
        let mut dup_exts = Vec::with_capacity(ext_inner.len() * 2);
        dup_exts.extend_from_slice(ext_inner);
        dup_exts.extend_from_slice(ext_inner);
        let mut dup_tbs_inner = Vec::new();
        for part in [v_tlv, s_tlv, a_tlv, i_tlv, val_tlv, sub_tlv, spki_tlv] {
            dup_tbs_inner.extend_from_slice(part);
        }
        dup_tbs_inner.extend_from_slice(&der_tlv(0xa3, &der_tlv(0x30, &dup_exts)));
        let dup_tbs = der_tlv(0x30, &dup_tbs_inner);
        let dup_sig = tls_sk.sign(&dup_tbs).to_bytes();
        let mut sig_bits = vec![0x00];
        sig_bits.extend_from_slice(&dup_sig);
        let mut dup_cert_inner = dup_tbs;
        dup_cert_inner.extend_from_slice(a_tlv);
        dup_cert_inner.extend_from_slice(&der_tlv(0x03, &sig_bits));
        let dup_cert_der = der_tlv(0x30, &dup_cert_inner);
        assert_eq!(
            verify_node_x509_cert_der(&dup_cert_der, &signer, 5_000),
            Err(CertVerifyError::MalformedDer(
                "duplicate NLOSCERT Principal-binding extension"
            ))
        );
    }
}
