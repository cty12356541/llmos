//! Frozen cross-crate publisher trust types and [`PublisherTrustVerifier`]
//! trait shared across `nlos-publisher-trust`, `nlos-store-registry`, and
//! `nlos-certify` (`ADR-0027` section 2.3).

use std::error::Error;
use std::fmt;

use nlos_types::{ApplicationId, KeyId};

const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";

/// Stable 128-bit nominal identifier of a registered package or driver publisher
/// (`ADR-0027` section 2.3; `[PKG-APPID-001]`).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PublisherId([u8; 16]);

impl PublisherId {
    /// Constructs a [`PublisherId`] from raw 16-byte big-endian identity bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Consumes the identifier and returns its underlying 16-byte array.
    #[must_use]
    pub const fn into_bytes(self) -> [u8; 16] {
        self.0
    }

    /// Borrows the underlying 16-byte identity array.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Formats the identifier as a 32-character lowercase hexadecimal string.
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(32);
        for byte in self.0 {
            out.push(char::from(HEX_LOWER[usize::from(byte >> 4)]));
            out.push(char::from(HEX_LOWER[usize::from(byte & 0x0f)]));
        }
        out
    }

    /// Parses a [`PublisherId`] from a 32-character ASCII hexadecimal string.
    ///
    /// # Errors
    ///
    /// Returns [`PublisherIdParseError::InvalidLength`] if `hex.len() != 32`, or
    /// [`PublisherIdParseError::InvalidHexByte`] if any byte is outside
    /// `0..=9`, `a..=f`, and `A..=F`.
    pub fn from_hex(hex: &str) -> Result<Self, PublisherIdParseError> {
        let raw = hex.as_bytes();
        if raw.len() != 32 {
            return Err(PublisherIdParseError::InvalidLength { actual: raw.len() });
        }
        let mut bytes = [0u8; 16];
        for (slot_idx, slot) in bytes.iter_mut().enumerate() {
            let hi_index = slot_idx * 2;
            let lo_index = hi_index + 1;
            let hi =
                decode_hex_nibble(raw[hi_index]).ok_or(PublisherIdParseError::InvalidHexByte {
                    index: hi_index,
                    byte: raw[hi_index],
                })?;
            let lo =
                decode_hex_nibble(raw[lo_index]).ok_or(PublisherIdParseError::InvalidHexByte {
                    index: lo_index,
                    byte: raw[lo_index],
                })?;
            *slot = (hi << 4) | lo;
        }
        Ok(Self(bytes))
    }
}

const fn decode_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

impl fmt::Display for PublisherId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Typed parse failure when decoding a [`PublisherId`] from hexadecimal text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublisherIdParseError {
    /// Input byte length was not 32 hexadecimal ASCII characters.
    InvalidLength {
        /// Observed byte length of the input string.
        actual: usize,
    },
    /// A byte at `index` was not an ASCII hexadecimal digit.
    InvalidHexByte {
        /// Zero-based byte offset in the input string.
        index: usize,
        /// Offending raw byte value.
        byte: u8,
    },
}

impl fmt::Display for PublisherIdParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength { actual } => write!(
                formatter,
                "invalid PublisherId hex length: expected 32 ASCII chars, got {actual}"
            ),
            Self::InvalidHexByte { index, byte } => write!(
                formatter,
                "invalid PublisherId hex byte 0x{byte:02x} at index {index}"
            ),
        }
    }
}

impl Error for PublisherIdParseError {}

/// Three-tier publisher trust domain and sandbox isolation posture
/// (`ADR-0027` section 2.3).
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PublisherTrustTier {
    /// First-party platform component signed under the offline platform root CA.
    PlatformCore = 1,
    /// Third-party publisher verified by the store and certified across all four
    /// `nlos-certify` gates.
    CertifiedPublisher = 2,
    /// Self-signed or unendorsed community package confined to the dual-layer sandbox.
    CommunitySandboxed = 3,
}

impl PublisherTrustTier {
    /// Returns the stable one-byte wire and storage discriminant (`1..=3`).
    #[must_use]
    pub const fn encode(self) -> u8 {
        self as u8
    }

    /// Decodes a `SQLite` integer column value into a closed [`PublisherTrustTier`].
    #[must_use]
    pub const fn decode(value: i64) -> Option<Self> {
        match value {
            1 => Some(Self::PlatformCore),
            2 => Some(Self::CertifiedPublisher),
            3 => Some(Self::CommunitySandboxed),
            _ => None,
        }
    }

    /// Returns the canonical `ADR-0027` section 2.3 uppercase identifier string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PlatformCore => "PLATFORM_CORE",
            Self::CertifiedPublisher => "CERTIFIED_PUBLISHER",
            Self::CommunitySandboxed => "COMMUNITY_SANDBOXED",
        }
    }
}

impl fmt::Display for PublisherTrustTier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Monotonically increasing certificate and advisory revocation epoch
/// (`ADR-0027` section 2.3).
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RevocationEpoch(pub u64);

impl RevocationEpoch {
    /// Wraps a raw monotonic epoch counter.
    #[must_use]
    pub const fn new(epoch: u64) -> Self {
        Self(epoch)
    }

    /// Returns the underlying `u64` epoch value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Advances the epoch by one, saturating at [`u64::MAX`] instead of wrapping.
    #[must_use]
    pub const fn saturating_next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl fmt::Display for RevocationEpoch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// Verified cryptographic publisher identity produced after certificate-chain,
/// revocation-epoch, expiry, and `Ed25519` signature validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerifiedPublisherIdentity {
    pub publisher_id: PublisherId,
    pub trust_tier: PublisherTrustTier,
    pub leaf_key_id: KeyId,
    pub leaf_public_key: [u8; 32],
    pub revocation_epoch: RevocationEpoch,
    pub verified_at_ms: u64,
}

/// Durable first-registration ownership binding between an [`ApplicationId`] and
/// its owning [`PublisherId`] (`[PKG-APPID-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApplicationOwnershipBinding {
    pub application_id: ApplicationId,
    pub owner_publisher_id: PublisherId,
    pub trust_tier: PublisherTrustTier,
    pub first_registered_at_ms: u64,
    pub registration_generation: u64,
}

/// Fail-closed verification errors emitted by [`PublisherTrustVerifier`]
/// implementations (`ADR-0027` section 2.3; `[PKG-IMM-001]`, `[PKG-APPID-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublisherTrustCheckError {
    UnknownPublisher(PublisherId),
    UntrustedCertificateChain(&'static str),
    ExpiredCertificate {
        expires_at_ms: u64,
        now_ms: u64,
    },
    RevokedCertificate {
        serial_or_key: [u8; 16],
        revoked_at_epoch: RevocationEpoch,
    },
    RevokedPublisher {
        publisher_id: PublisherId,
        revoked_at_epoch: RevocationEpoch,
    },
    StaleOrRollbackRevocationEpoch {
        current_epoch: RevocationEpoch,
        candidate_epoch: RevocationEpoch,
    },
    FrozenRevocationList {
        expires_at_ms: u64,
        now_ms: u64,
    },
    SignatureInvalid,
    ApplicationIdHijackAttempt {
        application_id: ApplicationId,
        registered_owner: PublisherId,
        attempted_publisher: PublisherId,
    },
    StorageFailure(String),
}

impl fmt::Display for PublisherTrustCheckError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPublisher(publisher_id) => {
                write!(formatter, "unknown publisher {publisher_id}")
            }
            Self::UntrustedCertificateChain(reason) => {
                write!(formatter, "untrusted publisher certificate chain: {reason}")
            }
            Self::ExpiredCertificate {
                expires_at_ms,
                now_ms,
            } => write!(
                formatter,
                "publisher certificate expired at {expires_at_ms}ms (now {now_ms}ms)"
            ),
            Self::RevokedCertificate {
                serial_or_key,
                revoked_at_epoch,
            } => {
                formatter.write_str("publisher certificate ")?;
                for byte in serial_or_key {
                    write!(formatter, "{byte:02x}")?;
                }
                write!(formatter, " revoked at epoch {revoked_at_epoch}")
            }
            Self::RevokedPublisher {
                publisher_id,
                revoked_at_epoch,
            } => write!(
                formatter,
                "publisher {publisher_id} revoked at epoch {revoked_at_epoch}"
            ),
            Self::StaleOrRollbackRevocationEpoch {
                current_epoch,
                candidate_epoch,
            } => write!(
                formatter,
                "stale or rolled-back revocation epoch: current={current_epoch}, candidate={candidate_epoch}"
            ),
            Self::FrozenRevocationList {
                expires_at_ms,
                now_ms,
            } => write!(
                formatter,
                "revocation list freshness window expired at {expires_at_ms}ms (now {now_ms}ms)"
            ),
            Self::SignatureInvalid => {
                formatter.write_str("publisher Ed25519 signature verification failed")
            }
            Self::ApplicationIdHijackAttempt {
                application_id,
                registered_owner,
                attempted_publisher,
            } => write!(
                formatter,
                "ApplicationId hijack refused for {application_id:?}: owned by {registered_owner}, attempted by {attempted_publisher} (PKG-APPID-001)"
            ),
            Self::StorageFailure(detail) => {
                write!(formatter, "publisher trust storage failure: {detail}")
            }
        }
    }
}

impl Error for PublisherTrustCheckError {}

/// Cross-crate contract consulted by `nlos-store-registry`, `nlos-certify`, and
/// package installation gates to verify publisher signatures, revocation freshness,
/// and anti-hijack [`ApplicationId`] ownership bindings.
pub trait PublisherTrustVerifier {
    /// Returns the authority's current monotonic [`RevocationEpoch`].
    ///
    /// # Errors
    ///
    /// Returns [`PublisherTrustCheckError`] if the underlying trust store cannot
    /// read or validate the active revocation epoch.
    fn current_revocation_epoch(&self) -> Result<RevocationEpoch, PublisherTrustCheckError>;

    /// Verifies a 64-byte `Ed25519` signature over a 32-byte digest against the
    /// publisher's active certificate chain and revocation state at `now_ms`.
    ///
    /// # Errors
    ///
    /// Returns [`PublisherTrustCheckError`] when the publisher is unknown, the
    /// certificate chain is untrusted, expired, or revoked, the revocation list
    /// freshness window has elapsed, the signature is invalid, or storage fails.
    fn verify_publisher_signature(
        &self,
        publisher_id: PublisherId,
        signing_public_key: &[u8; 32],
        message_digest: &[u8; 32],
        signature: &[u8; 64],
        now_ms: u64,
    ) -> Result<VerifiedPublisherIdentity, PublisherTrustCheckError>;

    /// Verifies that `publisher_id` owns `application_id`, or atomically binds
    /// ownership on first legal registration (`[PKG-APPID-001]`).
    ///
    /// # Errors
    ///
    /// Returns [`PublisherTrustCheckError::ApplicationIdHijackAttempt`] if another
    /// publisher already owns `application_id`, or another
    /// [`PublisherTrustCheckError`] variant on trust or storage failure.
    fn verify_or_bind_application_owner(
        &self,
        publisher_id: PublisherId,
        trust_tier: PublisherTrustTier,
        application_id: ApplicationId,
        now_ms: u64,
    ) -> Result<ApplicationOwnershipBinding, PublisherTrustCheckError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubVerifier {
        epoch: RevocationEpoch,
        known_publisher: PublisherId,
        known_pubkey: [u8; 32],
        owner_app: ApplicationId,
    }

    impl PublisherTrustVerifier for StubVerifier {
        fn current_revocation_epoch(&self) -> Result<RevocationEpoch, PublisherTrustCheckError> {
            Ok(self.epoch)
        }

        fn verify_publisher_signature(
            &self,
            publisher_id: PublisherId,
            signing_public_key: &[u8; 32],
            _message_digest: &[u8; 32],
            signature: &[u8; 64],
            now_ms: u64,
        ) -> Result<VerifiedPublisherIdentity, PublisherTrustCheckError> {
            if publisher_id != self.known_publisher {
                return Err(PublisherTrustCheckError::UnknownPublisher(publisher_id));
            }
            if signing_public_key != &self.known_pubkey || signature[0] == 0 {
                return Err(PublisherTrustCheckError::SignatureInvalid);
            }
            Ok(VerifiedPublisherIdentity {
                publisher_id,
                trust_tier: PublisherTrustTier::CertifiedPublisher,
                leaf_key_id: KeyId::from_bytes([0x11; 16]),
                leaf_public_key: *signing_public_key,
                revocation_epoch: self.epoch,
                verified_at_ms: now_ms,
            })
        }

        fn verify_or_bind_application_owner(
            &self,
            publisher_id: PublisherId,
            trust_tier: PublisherTrustTier,
            application_id: ApplicationId,
            now_ms: u64,
        ) -> Result<ApplicationOwnershipBinding, PublisherTrustCheckError> {
            if application_id == self.owner_app && publisher_id != self.known_publisher {
                return Err(PublisherTrustCheckError::ApplicationIdHijackAttempt {
                    application_id,
                    registered_owner: self.known_publisher,
                    attempted_publisher: publisher_id,
                });
            }
            Ok(ApplicationOwnershipBinding {
                application_id,
                owner_publisher_id: publisher_id,
                trust_tier,
                first_registered_at_ms: now_ms,
                registration_generation: 1,
            })
        }
    }

    #[test]
    fn publisher_id_hex_roundtrip_and_parse_errors() {
        let raw = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10,
        ];
        let id = PublisherId::from_bytes(raw);
        assert_eq!(id.as_bytes(), &raw);
        assert_eq!(id.into_bytes(), raw);

        let hex = id.to_hex();
        assert_eq!(hex, "0123456789abcdeffedcba9876543210");
        assert_eq!(id.to_string(), hex);
        assert_eq!(PublisherId::from_hex(&hex), Ok(id));
        assert_eq!(PublisherId::from_hex(&hex.to_ascii_uppercase()), Ok(id));

        assert_eq!(
            PublisherId::from_hex("0123"),
            Err(PublisherIdParseError::InvalidLength { actual: 4 })
        );
        let invalid_err = PublisherId::from_hex("0123456789abcdeffedcba987654321g").unwrap_err();
        assert_eq!(
            invalid_err,
            PublisherIdParseError::InvalidHexByte {
                index: 31,
                byte: b'g',
            }
        );
        assert!(invalid_err.to_string().contains("index 31"));
    }

    #[test]
    fn trust_tier_encoding_decoding_and_display_match_adr_0027() {
        let tiers = [
            (
                1_i64,
                1_u8,
                PublisherTrustTier::PlatformCore,
                "PLATFORM_CORE",
            ),
            (
                2_i64,
                2_u8,
                PublisherTrustTier::CertifiedPublisher,
                "CERTIFIED_PUBLISHER",
            ),
            (
                3_i64,
                3_u8,
                PublisherTrustTier::CommunitySandboxed,
                "COMMUNITY_SANDBOXED",
            ),
        ];

        for (sql_val, wire_val, tier, expected_str) in tiers {
            assert_eq!(tier.encode(), wire_val);
            assert_eq!(PublisherTrustTier::decode(sql_val), Some(tier));
            assert_eq!(tier.as_str(), expected_str);
            assert_eq!(tier.to_string(), expected_str);
        }

        assert_eq!(PublisherTrustTier::decode(0), None);
        assert_eq!(PublisherTrustTier::decode(4), None);
        assert_eq!(PublisherTrustTier::decode(-1), None);
        assert!(PublisherTrustTier::PlatformCore < PublisherTrustTier::CertifiedPublisher);
        assert!(PublisherTrustTier::CertifiedPublisher < PublisherTrustTier::CommunitySandboxed);
    }

    #[test]
    fn revocation_epoch_monotonic_progression_and_saturation() {
        let initial = RevocationEpoch::default();
        assert_eq!(initial.get(), 0);
        assert_eq!(initial.to_string(), "0");

        let next = initial.saturating_next();
        assert_eq!(next, RevocationEpoch::new(1));
        assert!(next > initial);

        let max_epoch = RevocationEpoch::new(u64::MAX);
        assert_eq!(max_epoch.saturating_next(), max_epoch);
    }

    #[test]
    fn verifier_contract_and_error_formatting_cover_all_variants() {
        let owner = PublisherId::from_bytes([0xaa; 16]);
        let attacker = PublisherId::from_bytes([0xbb; 16]);
        let app_id = ApplicationId::from_bytes([0xcc; 16]);
        let pubkey = [0x22; 32];
        let digest = [0x33; 32];
        let valid_sig = [0x44; 64];

        let verifier = StubVerifier {
            epoch: RevocationEpoch::new(7),
            known_publisher: owner,
            known_pubkey: pubkey,
            owner_app: app_id,
        };

        assert_eq!(
            verifier.current_revocation_epoch(),
            Ok(RevocationEpoch::new(7))
        );

        let identity = verifier
            .verify_publisher_signature(owner, &pubkey, &digest, &valid_sig, 1_700_000_000_000)
            .expect("known signature should verify");
        assert_eq!(identity.publisher_id, owner);
        assert_eq!(identity.trust_tier, PublisherTrustTier::CertifiedPublisher);
        assert_eq!(identity.revocation_epoch, RevocationEpoch::new(7));

        let binding = verifier
            .verify_or_bind_application_owner(
                owner,
                PublisherTrustTier::CertifiedPublisher,
                app_id,
                1_700_000_000_000,
            )
            .expect("owner binding should succeed");
        assert_eq!(binding.owner_publisher_id, owner);
        assert_eq!(binding.registration_generation, 1);

        let hijack = verifier
            .verify_or_bind_application_owner(
                attacker,
                PublisherTrustTier::CommunitySandboxed,
                app_id,
                1_700_000_000_100,
            )
            .expect_err("hijack attempt must fail closed");
        assert_eq!(
            hijack,
            PublisherTrustCheckError::ApplicationIdHijackAttempt {
                application_id: app_id,
                registered_owner: owner,
                attempted_publisher: attacker,
            }
        );

        let all_errors: [PublisherTrustCheckError; 10] = [
            PublisherTrustCheckError::UnknownPublisher(attacker),
            PublisherTrustCheckError::UntrustedCertificateChain("root mismatch"),
            PublisherTrustCheckError::ExpiredCertificate {
                expires_at_ms: 10,
                now_ms: 20,
            },
            PublisherTrustCheckError::RevokedCertificate {
                serial_or_key: [0x55; 16],
                revoked_at_epoch: RevocationEpoch::new(3),
            },
            PublisherTrustCheckError::RevokedPublisher {
                publisher_id: attacker,
                revoked_at_epoch: RevocationEpoch::new(4),
            },
            PublisherTrustCheckError::StaleOrRollbackRevocationEpoch {
                current_epoch: RevocationEpoch::new(7),
                candidate_epoch: RevocationEpoch::new(6),
            },
            PublisherTrustCheckError::FrozenRevocationList {
                expires_at_ms: 100,
                now_ms: 200,
            },
            PublisherTrustCheckError::SignatureInvalid,
            hijack,
            PublisherTrustCheckError::StorageFailure("disk I/O".to_owned()),
        ];

        for err in &all_errors {
            let dyn_err: &dyn Error = err;
            assert_ne!(dyn_err.to_string(), "");
        }
    }
}
