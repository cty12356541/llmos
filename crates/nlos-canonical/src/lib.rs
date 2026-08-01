//! Strict deterministic CBOR for NLOS signature preimages.
//!
//! This encoding is deliberately separate from the Protobuf RPC wire format.
//! A verifier must supply the expected [`SignatureDomain`]; decoding bytes
//! without checking their intended domain is not a valid signature check.

use std::error::Error;
use std::fmt;

use minicbor::{Decoder, Encoder};

pub const SCHEMA_NAME: &str = "nlos.canonical.DigestEnvelope";
pub const SCHEMA_MAJOR: u32 = 1;
pub const SCHEMA_MINOR: u32 = 0;
pub const PAYLOAD_DIGEST_ALGORITHM: &str = "sha-256";
pub const MAX_CANONICAL_BYTES: usize = 4096;
pub const MAX_DOMAIN_BYTES: usize = 96;
pub const MAX_EXTENSIONS_PER_CLASS: usize = 16;
pub const MAX_EXTENSION_VALUE_BYTES: usize = 256;
pub const OBJECT_ID_BYTES: usize = 16;
pub const PAYLOAD_DIGEST_BYTES: usize = 32;

const FIELD_COUNT: u64 = 8;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SignatureDomain(String);

impl SignatureDomain {
    /// Creates an ASCII domain identifier such as `nlos.receipt/v1`.
    ///
    /// # Errors
    ///
    /// Returns [`CanonicalError::InvalidDomain`] for an empty, oversized, or
    /// non-ASCII identifier, or for characters outside `[a-z0-9._/-]`.
    pub fn new(value: impl Into<String>) -> Result<Self, CanonicalError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= MAX_DOMAIN_BYTES
            && value.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._/-".contains(&byte)
            });
        if !valid {
            return Err(CanonicalError::InvalidDomain);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Extension {
    id: u32,
    value: Vec<u8>,
}

impl Extension {
    /// Creates an opaque signed extension value.
    ///
    /// # Errors
    ///
    /// Returns [`CanonicalError::ExtensionValueTooLarge`] when the value
    /// exceeds [`MAX_EXTENSION_VALUE_BYTES`].
    pub fn new(id: u32, value: impl Into<Vec<u8>>) -> Result<Self, CanonicalError> {
        let value = value.into();
        if value.len() > MAX_EXTENSION_VALUE_BYTES {
            return Err(CanonicalError::ExtensionValueTooLarge {
                id,
                actual: value.len(),
            });
        }
        Ok(Self { id, value })
    }

    #[must_use]
    pub const fn id(&self) -> u32 {
        self.id
    }

    #[must_use]
    pub fn value(&self) -> &[u8] {
        &self.value
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CanonicalObjectId([u8; OBJECT_ID_BYTES]);

impl CanonicalObjectId {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; OBJECT_ID_BYTES]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; OBJECT_ID_BYTES] {
        &self.0
    }

    #[must_use]
    pub const fn into_bytes(self) -> [u8; OBJECT_ID_BYTES] {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Sha256Digest([u8; PAYLOAD_DIGEST_BYTES]);

impl Sha256Digest {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; PAYLOAD_DIGEST_BYTES]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; PAYLOAD_DIGEST_BYTES] {
        &self.0
    }

    #[must_use]
    pub const fn into_bytes(self) -> [u8; PAYLOAD_DIGEST_BYTES] {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalDigestEnvelope {
    schema_minor: u32,
    object_id: CanonicalObjectId,
    payload_digest: Sha256Digest,
    critical_extensions: Vec<Extension>,
    noncritical_extensions: Vec<Extension>,
}

impl CanonicalDigestEnvelope {
    /// Creates a canonical signing envelope.
    ///
    /// Extension slices must be strictly ordered by ID. This makes duplicate
    /// IDs and producer-dependent map ordering impossible at encode time.
    ///
    /// # Errors
    ///
    /// Returns [`CanonicalError`] when an extension class exceeds its bound,
    /// contains duplicate IDs, or is not strictly ordered.
    pub fn new(
        object_id: CanonicalObjectId,
        payload_digest: Sha256Digest,
        critical_extensions: Vec<Extension>,
        noncritical_extensions: Vec<Extension>,
    ) -> Result<Self, CanonicalError> {
        validate_extensions(&critical_extensions, ExtensionClass::Critical)?;
        validate_extensions(&noncritical_extensions, ExtensionClass::Noncritical)?;
        Ok(Self {
            schema_minor: SCHEMA_MINOR,
            object_id,
            payload_digest,
            critical_extensions,
            noncritical_extensions,
        })
    }

    #[must_use]
    pub const fn schema_minor(&self) -> u32 {
        self.schema_minor
    }

    #[must_use]
    pub const fn object_id(&self) -> CanonicalObjectId {
        self.object_id
    }

    #[must_use]
    pub const fn payload_digest(&self) -> Sha256Digest {
        self.payload_digest
    }

    #[must_use]
    pub fn critical_extensions(&self) -> &[Extension] {
        &self.critical_extensions
    }

    #[must_use]
    pub fn noncritical_extensions(&self) -> &[Extension] {
        &self.noncritical_extensions
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExtensionClass {
    Critical,
    Noncritical,
}

impl fmt::Display for ExtensionClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Critical => formatter.write_str("critical"),
            Self::Noncritical => formatter.write_str("noncritical"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CanonicalError {
    FrameTooLarge {
        actual: usize,
        maximum: usize,
    },
    Malformed(String),
    IndefiniteLength,
    WrongFieldCount {
        actual: u64,
        expected: u64,
    },
    DuplicateField(u8),
    FieldOrder {
        previous: u8,
        current: u8,
    },
    UnknownField(u8),
    MissingField(u8),
    WrongSchema(String),
    UnsupportedMajor(u32),
    UnsupportedDigestAlgorithm(String),
    InvalidDomain,
    DomainMismatch {
        expected: String,
        actual: String,
    },
    InvalidObjectIdLength(usize),
    InvalidPayloadDigestLength(usize),
    TooManyExtensions {
        class: &'static str,
        actual: usize,
    },
    DuplicateExtension {
        class: &'static str,
        id: u32,
    },
    ExtensionOrder {
        class: &'static str,
        previous: u32,
        current: u32,
    },
    ExtensionValueTooLarge {
        id: u32,
        actual: usize,
    },
    UnsupportedCriticalExtension(u32),
    TrailingData,
    NonCanonicalEncoding,
    Encoding(String),
    LengthOverflow,
}

impl fmt::Display for CanonicalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl Error for CanonicalError {}

/// Encodes the only accepted deterministic representation of an envelope.
///
/// # Errors
///
/// Returns [`CanonicalError::FrameTooLarge`] if bounded extensions make the
/// final envelope exceed [`MAX_CANONICAL_BYTES`], or an encoding error if the
/// underlying infallible vector writer unexpectedly fails.
pub fn encode(envelope: &CanonicalDigestEnvelope) -> Result<Vec<u8>, CanonicalError> {
    let mut encoder = Encoder::new(Vec::new());
    encode_into(&mut encoder, envelope)?;
    let bytes = encoder.into_writer();
    if bytes.len() > MAX_CANONICAL_BYTES {
        return Err(CanonicalError::FrameTooLarge {
            actual: bytes.len(),
            maximum: MAX_CANONICAL_BYTES,
        });
    }
    Ok(bytes)
}

/// Prepends an unambiguous signature domain to a canonical CBOR body.
///
/// The returned preimage is `u32_be(domain_len) || domain ||
/// u32_be(cbor_len) || cbor`. NLOS v0.5 hashes this complete preimage with
/// SHA-256; this crate does not perform key management or signing.
///
/// # Errors
///
/// Returns [`CanonicalError`] if the CBOR body cannot be encoded.
pub fn encode_signing_preimage(
    domain: &SignatureDomain,
    envelope: &CanonicalDigestEnvelope,
) -> Result<Vec<u8>, CanonicalError> {
    let body = encode(envelope)?;
    let mut preimage = Vec::with_capacity(8 + domain.as_str().len() + body.len());
    let domain_len =
        u32::try_from(domain.as_str().len()).map_err(|_| CanonicalError::LengthOverflow)?;
    let body_len = u32::try_from(body.len()).map_err(|_| CanonicalError::LengthOverflow)?;
    preimage.extend_from_slice(&domain_len.to_be_bytes());
    preimage.extend_from_slice(domain.as_str().as_bytes());
    preimage.extend_from_slice(&body_len.to_be_bytes());
    preimage.extend_from_slice(&body);
    Ok(preimage)
}

/// Strictly decodes a deterministic CBOR envelope.
///
/// Unknown noncritical extensions are returned as opaque bytes. Every
/// critical extension must be listed in `supported_critical_extensions`.
///
/// # Errors
///
/// Returns [`CanonicalError`] for malformed, non-deterministic, out-of-domain,
/// unsupported, oversized, duplicated, or trailing input.
pub fn decode(
    bytes: &[u8],
    supported_critical_extensions: &[u32],
) -> Result<CanonicalDigestEnvelope, CanonicalError> {
    if bytes.len() > MAX_CANONICAL_BYTES {
        return Err(CanonicalError::FrameTooLarge {
            actual: bytes.len(),
            maximum: MAX_CANONICAL_BYTES,
        });
    }

    let envelope = decode_structural(bytes)?;
    if let Some(extension) = envelope
        .critical_extensions
        .iter()
        .find(|extension| !supported_critical_extensions.contains(&extension.id))
    {
        return Err(CanonicalError::UnsupportedCriticalExtension(extension.id));
    }
    if encode(&envelope)? != bytes {
        return Err(CanonicalError::NonCanonicalEncoding);
    }
    Ok(envelope)
}

/// Verifies the length-prefixed domain and decodes its canonical CBOR body.
///
/// # Errors
///
/// Returns [`CanonicalError`] for a malformed prefix, a mismatched domain, a
/// length mismatch, or any error returned by [`decode`].
pub fn decode_signing_preimage_for_domain(
    preimage: &[u8],
    expected_domain: &SignatureDomain,
    supported_critical_extensions: &[u32],
) -> Result<CanonicalDigestEnvelope, CanonicalError> {
    if preimage.len() < 8 {
        return Err(CanonicalError::Malformed(
            "signing preimage is shorter than its two length prefixes".to_owned(),
        ));
    }
    let domain_len = u32::from_be_bytes(
        preimage[0..4]
            .try_into()
            .map_err(|_| CanonicalError::Malformed("invalid domain length prefix".to_owned()))?,
    ) as usize;
    let domain_end = 4_usize
        .checked_add(domain_len)
        .ok_or_else(|| CanonicalError::Malformed("domain length overflow".to_owned()))?;
    let body_length_end = domain_end
        .checked_add(4)
        .ok_or_else(|| CanonicalError::Malformed("body length offset overflow".to_owned()))?;
    if body_length_end > preimage.len() {
        return Err(CanonicalError::Malformed(
            "domain length exceeds signing preimage".to_owned(),
        ));
    }
    let actual_domain = std::str::from_utf8(&preimage[4..domain_end])
        .map_err(|error| CanonicalError::Malformed(error.to_string()))?;
    if actual_domain != expected_domain.as_str() {
        return Err(CanonicalError::DomainMismatch {
            expected: expected_domain.as_str().to_owned(),
            actual: actual_domain.to_owned(),
        });
    }
    let body_len = u32::from_be_bytes(
        preimage[domain_end..body_length_end]
            .try_into()
            .map_err(|_| CanonicalError::Malformed("invalid body length prefix".to_owned()))?,
    ) as usize;
    let body_end = body_length_end
        .checked_add(body_len)
        .ok_or_else(|| CanonicalError::Malformed("body length overflow".to_owned()))?;
    if body_end != preimage.len() {
        return Err(CanonicalError::TrailingData);
    }
    decode(
        &preimage[body_length_end..body_end],
        supported_critical_extensions,
    )
}

fn encode_into(
    encoder: &mut Encoder<Vec<u8>>,
    envelope: &CanonicalDigestEnvelope,
) -> Result<(), CanonicalError> {
    encoder
        .map(FIELD_COUNT)
        .and_then(|encoder| encoder.u8(0))
        .and_then(|encoder| encoder.str(SCHEMA_NAME))
        .and_then(|encoder| encoder.u8(1))
        .and_then(|encoder| encoder.u32(SCHEMA_MAJOR))
        .and_then(|encoder| encoder.u8(2))
        .and_then(|encoder| encoder.u32(envelope.schema_minor))
        .and_then(|encoder| encoder.u8(3))
        .and_then(|encoder| encoder.str(PAYLOAD_DIGEST_ALGORITHM))
        .and_then(|encoder| encoder.u8(4))
        .and_then(|encoder| encoder.bytes(envelope.object_id.as_bytes()))
        .and_then(|encoder| encoder.u8(5))
        .and_then(|encoder| encoder.bytes(envelope.payload_digest.as_bytes()))
        .and_then(|encoder| encoder.u8(6))
        .map_err(|error| CanonicalError::Encoding(error.to_string()))?;
    encode_extensions(encoder, &envelope.critical_extensions)?;
    encoder
        .u8(7)
        .map_err(|error| CanonicalError::Encoding(error.to_string()))?;
    encode_extensions(encoder, &envelope.noncritical_extensions)
}

fn encode_extensions(
    encoder: &mut Encoder<Vec<u8>>,
    extensions: &[Extension],
) -> Result<(), CanonicalError> {
    encoder
        .map(extensions.len() as u64)
        .map_err(|error| CanonicalError::Encoding(error.to_string()))?;
    for extension in extensions {
        encoder
            .u32(extension.id)
            .and_then(|encoder| encoder.bytes(&extension.value))
            .map_err(|error| CanonicalError::Encoding(error.to_string()))?;
    }
    Ok(())
}

fn decode_structural(bytes: &[u8]) -> Result<CanonicalDigestEnvelope, CanonicalError> {
    let mut decoder = Decoder::new(bytes);
    let fields = decoder
        .map()
        .map_err(decode_error)?
        .ok_or(CanonicalError::IndefiniteLength)?;
    if fields != FIELD_COUNT {
        return Err(CanonicalError::WrongFieldCount {
            actual: fields,
            expected: FIELD_COUNT,
        });
    }

    let mut seen = 0_u16;
    let mut previous_key = None;
    let mut schema_name = None;
    let mut major = None;
    let mut minor = None;
    let mut digest_algorithm = None;
    let mut object_id = None;
    let mut payload_digest = None;
    let mut critical_extensions = None;
    let mut noncritical_extensions = None;

    for _ in 0..fields {
        let key = decoder.u8().map_err(decode_error)?;
        if key > 7 {
            return Err(CanonicalError::UnknownField(key));
        }
        let bit = 1_u16 << key;
        if seen & bit != 0 {
            return Err(CanonicalError::DuplicateField(key));
        }
        if let Some(previous) = previous_key
            && key < previous
        {
            return Err(CanonicalError::FieldOrder {
                previous,
                current: key,
            });
        }
        seen |= bit;
        previous_key = Some(key);

        match key {
            0 => schema_name = Some(decoder.str().map_err(decode_error)?.to_owned()),
            1 => major = Some(decoder.u32().map_err(decode_error)?),
            2 => minor = Some(decoder.u32().map_err(decode_error)?),
            3 => digest_algorithm = Some(decoder.str().map_err(decode_error)?.to_owned()),
            4 => object_id = Some(decoder.bytes().map_err(decode_error)?.to_vec()),
            5 => payload_digest = Some(decoder.bytes().map_err(decode_error)?.to_vec()),
            6 => {
                critical_extensions =
                    Some(decode_extensions(&mut decoder, ExtensionClass::Critical)?);
            }
            7 => {
                noncritical_extensions = Some(decode_extensions(
                    &mut decoder,
                    ExtensionClass::Noncritical,
                )?);
            }
            _ => return Err(CanonicalError::UnknownField(key)),
        }
    }

    if decoder.position() != bytes.len() {
        return Err(CanonicalError::TrailingData);
    }
    let schema_name = schema_name.ok_or(CanonicalError::MissingField(0))?;
    if schema_name != SCHEMA_NAME {
        return Err(CanonicalError::WrongSchema(schema_name));
    }
    let major = major.ok_or(CanonicalError::MissingField(1))?;
    if major != SCHEMA_MAJOR {
        return Err(CanonicalError::UnsupportedMajor(major));
    }
    let minor = minor.ok_or(CanonicalError::MissingField(2))?;
    let digest_algorithm = digest_algorithm.ok_or(CanonicalError::MissingField(3))?;
    if digest_algorithm != PAYLOAD_DIGEST_ALGORITHM {
        return Err(CanonicalError::UnsupportedDigestAlgorithm(digest_algorithm));
    }
    let object_id = CanonicalObjectId::from_bytes(fixed_array::<OBJECT_ID_BYTES>(
        object_id.ok_or(CanonicalError::MissingField(4))?,
        CanonicalError::InvalidObjectIdLength,
    )?);
    let payload_digest = Sha256Digest::from_bytes(fixed_array::<PAYLOAD_DIGEST_BYTES>(
        payload_digest.ok_or(CanonicalError::MissingField(5))?,
        CanonicalError::InvalidPayloadDigestLength,
    )?);

    let mut envelope = CanonicalDigestEnvelope::new(
        object_id,
        payload_digest,
        critical_extensions.ok_or(CanonicalError::MissingField(6))?,
        noncritical_extensions.ok_or(CanonicalError::MissingField(7))?,
    )?;
    envelope.schema_minor = minor;
    Ok(envelope)
}

fn decode_extensions(
    decoder: &mut Decoder<'_>,
    class: ExtensionClass,
) -> Result<Vec<Extension>, CanonicalError> {
    let count = decoder
        .map()
        .map_err(decode_error)?
        .ok_or(CanonicalError::IndefiniteLength)?;
    let count = usize::try_from(count).map_err(|_| CanonicalError::TooManyExtensions {
        class: class_name(class),
        actual: usize::MAX,
    })?;
    if count > MAX_EXTENSIONS_PER_CLASS {
        return Err(CanonicalError::TooManyExtensions {
            class: class_name(class),
            actual: count,
        });
    }
    let mut extensions = Vec::with_capacity(count);
    let mut previous = None;
    for _ in 0..count {
        let id = decoder.u32().map_err(decode_error)?;
        if let Some(previous) = previous {
            if id == previous {
                return Err(CanonicalError::DuplicateExtension {
                    class: class_name(class),
                    id,
                });
            }
            if id < previous {
                return Err(CanonicalError::ExtensionOrder {
                    class: class_name(class),
                    previous,
                    current: id,
                });
            }
        }
        let value = decoder.bytes().map_err(decode_error)?.to_vec();
        extensions.push(Extension::new(id, value)?);
        previous = Some(id);
    }
    Ok(extensions)
}

fn validate_extensions(
    extensions: &[Extension],
    class: ExtensionClass,
) -> Result<(), CanonicalError> {
    if extensions.len() > MAX_EXTENSIONS_PER_CLASS {
        return Err(CanonicalError::TooManyExtensions {
            class: class_name(class),
            actual: extensions.len(),
        });
    }
    for pair in extensions.windows(2) {
        if pair[0].id == pair[1].id {
            return Err(CanonicalError::DuplicateExtension {
                class: class_name(class),
                id: pair[0].id,
            });
        }
        if pair[0].id > pair[1].id {
            return Err(CanonicalError::ExtensionOrder {
                class: class_name(class),
                previous: pair[0].id,
                current: pair[1].id,
            });
        }
    }
    Ok(())
}

const fn class_name(class: ExtensionClass) -> &'static str {
    match class {
        ExtensionClass::Critical => "critical",
        ExtensionClass::Noncritical => "noncritical",
    }
}

fn fixed_array<const N: usize>(
    value: Vec<u8>,
    error: fn(usize) -> CanonicalError,
) -> Result<[u8; N], CanonicalError> {
    let actual = value.len();
    value.try_into().map_err(|_| error(actual))
}

fn decode_error(error: minicbor::decode::Error) -> CanonicalError {
    let message = error.to_string();
    drop(error);
    CanonicalError::Malformed(message)
}
