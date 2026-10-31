//! Content-addressed `OCI` artifact layer verifier and local `CAS` directory
//! (`ADR-0027` section 2.3.3; `[PKG-OCI-001]`).
//!
//! Architectural invariant (`[PKG-OCI-001]`): `OCI` blobs and layouts provide
//! immutable content-addressed transport and storage only. Writing layers or an
//! [`OciPackageLayout`] into [`LocalOciCasStore`] **never** creates a store
//! catalog listing, channel promotion, or installation entitlement.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use crate::error::{OciCasError, hex_32};

/// Canonical `OCI` image/artifact manifest schema version used by NLOS packages.
pub const OCI_IMAGE_MANIFEST_SCHEMA_V2: u32 = 2;

/// Standard NLOS package config `media_type` in `OCI` artifact layouts.
pub const NLOS_OCI_CONFIG_MEDIA_TYPE: &str = "application/vnd.nlos.package.config.v1+json";

const OCI_MANIFEST_DIGEST_DOMAIN: &[u8] = b"nlos/oci-cas/package-manifest/v1";
const OCI_MANIFEST_RECORD_HEADER: &str = "nlos.oci.manifest.v1";

static TEMP_NONCE: AtomicU64 = AtomicU64::new(1);

/// Content-addressed `OCI` artifact layer descriptor (`[PKG-OCI-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OciLayerDescriptor {
    /// `OCI` layer `media_type` (for example `application/vnd.nlos.component.wasm.v1`).
    pub media_type: String,
    /// Exact `SHA-256` digest of the uncompressed/archived layer byte stream.
    pub sha256_digest: [u8; 32],
    /// Exact byte length of the layer stream.
    pub size_bytes: u64,
}

impl OciLayerDescriptor {
    /// Computes an [`OciLayerDescriptor`] directly from a verified `media_type`
    /// and raw layer byte slice.
    ///
    /// # Errors
    ///
    /// Returns [`OciCasError::InvalidMediaType`] if `media_type` is empty or
    /// contains ASCII control/whitespace characters.
    pub fn from_layer_bytes(
        media_type: impl Into<String>,
        raw_bytes: &[u8],
    ) -> Result<Self, OciCasError> {
        let media_type = media_type.into();
        validate_media_type(&media_type)?;
        let size_bytes =
            u64::try_from(raw_bytes.len()).map_err(|_| OciCasError::LayerSizeMismatch {
                declared: u64::MAX,
                actual: 0,
            })?;
        let sha256_digest = compute_sha256(raw_bytes);
        Ok(Self {
            media_type,
            sha256_digest,
            size_bytes,
        })
    }

    /// Verifies that `raw_bytes` matches `self.size_bytes` and
    /// `self.sha256_digest` bitwise.
    ///
    /// # Errors
    ///
    /// Fails closed on invalid `media_type`, byte-length mismatch, or `SHA-256`
    /// digest tampering.
    pub fn verify_bytes(&self, raw_bytes: &[u8]) -> Result<(), OciCasError> {
        validate_media_type(&self.media_type)?;
        let actual_size =
            u64::try_from(raw_bytes.len()).map_err(|_| OciCasError::LayerSizeMismatch {
                declared: self.size_bytes,
                actual: u64::MAX,
            })?;
        if actual_size != self.size_bytes {
            return Err(OciCasError::LayerSizeMismatch {
                declared: self.size_bytes,
                actual: actual_size,
            });
        }
        let computed = compute_sha256(raw_bytes);
        if computed != self.sha256_digest {
            return Err(OciCasError::LayerDigestMismatch {
                declared: self.sha256_digest,
                computed,
            });
        }
        Ok(())
    }
}

/// Verified multi-layer `OCI` package layout and its canonical
/// `oci_manifest_digest` (`[PKG-OCI-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OciPackageLayout {
    /// `OCI` manifest schema version (`2`).
    pub schema_version: u32,
    /// Config descriptor (`media_type`, `sha256_digest`, `size_bytes`).
    pub config: OciLayerDescriptor,
    /// Ordered immutable component/asset layer descriptors.
    pub layers: Vec<OciLayerDescriptor>,
    /// Canonical domain-separated `SHA-256` digest over `(schema_version, config, layers)`.
    pub oci_manifest_digest: [u8; 32],
}

impl OciPackageLayout {
    /// Validates the config stream and every `(descriptor, raw_bytes)` pair
    /// bitwise before computing the canonical `oci_manifest_digest`.
    ///
    /// # Errors
    ///
    /// Fails closed if `layers` is empty, any `media_type` is malformed, or any
    /// layer's byte size or `SHA-256` digest mismatches its declared descriptor.
    pub fn from_verified_streams(
        config_descriptor: &OciLayerDescriptor,
        config_bytes: &[u8],
        layers: &[(OciLayerDescriptor, &[u8])],
    ) -> Result<Self, OciCasError> {
        if layers.is_empty() {
            return Err(OciCasError::EmptyLayers);
        }
        config_descriptor.verify_bytes(config_bytes)?;
        let mut verified_layers = Vec::with_capacity(layers.len());
        for (descriptor, payload) in layers {
            descriptor.verify_bytes(payload)?;
            verified_layers.push(descriptor.clone());
        }
        let oci_manifest_digest = compute_canonical_oci_manifest_digest(
            OCI_IMAGE_MANIFEST_SCHEMA_V2,
            config_descriptor,
            &verified_layers,
        );
        Ok(Self {
            schema_version: OCI_IMAGE_MANIFEST_SCHEMA_V2,
            config: config_descriptor.clone(),
            layers: verified_layers,
            oci_manifest_digest,
        })
    }
}

/// Computes the canonical domain-separated 32-byte `oci_manifest_digest` over
/// `schema_version`, `config`, and the ordered `layers` descriptors.
#[must_use]
pub fn compute_canonical_oci_manifest_digest(
    schema_version: u32,
    config: &OciLayerDescriptor,
    layers: &[OciLayerDescriptor],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(OCI_MANIFEST_DIGEST_DOMAIN);
    hasher.update(schema_version.to_be_bytes());
    hash_descriptor(&mut hasher, config);
    let count = u64::try_from(layers.len()).unwrap_or(u64::MAX);
    hasher.update(count.to_be_bytes());
    for layer in layers {
        hash_descriptor(&mut hasher, layer);
    }
    hasher.finalize().into()
}

fn hash_descriptor(hasher: &mut Sha256, descriptor: &OciLayerDescriptor) {
    let media_bytes = descriptor.media_type.as_bytes();
    let media_len = u64::try_from(media_bytes.len()).unwrap_or(u64::MAX);
    hasher.update(media_len.to_be_bytes());
    hasher.update(media_bytes);
    hasher.update(descriptor.sha256_digest);
    hasher.update(descriptor.size_bytes.to_be_bytes());
}

/// Local filesystem content-addressed `OCI` blob and manifest store
/// (`<cas_root>/blobs/sha256/<hex>` and `<cas_root>/manifests/sha256/<hex>`).
///
/// Strictly decoupled from [`crate::SqliteStoreRegistry`]: writing blobs or
/// layouts here never grants store visibility or installation authorization
/// (`[PKG-OCI-001]`).
#[derive(Clone, Debug)]
pub struct LocalOciCasStore {
    cas_root: PathBuf,
}

impl LocalOciCasStore {
    /// Opens or creates the local `OCI` `CAS` directory hierarchy at `cas_root`.
    ///
    /// # Errors
    ///
    /// Returns [`OciCasError::Io`] if `blobs/sha256` or `manifests/sha256`
    /// cannot be created.
    pub fn open(cas_root: impl AsRef<Path>) -> Result<Self, OciCasError> {
        let cas_root = cas_root.as_ref().to_path_buf();
        fs::create_dir_all(cas_root.join("blobs").join("sha256"))?;
        fs::create_dir_all(cas_root.join("manifests").join("sha256"))?;
        Ok(Self { cas_root })
    }

    /// Returns the root path of this `OCI` `CAS` directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.cas_root
    }

    /// Verifies `raw_bytes` against `descriptor` and atomically persists the
    /// immutable blob under `blobs/sha256/<digest_hex>`.
    ///
    /// # Errors
    ///
    /// Fails closed on size mismatch, `SHA-256` mismatch, corrupt existing on-disk
    /// blob, or filesystem write failure.
    pub fn put_layer_blob(
        &self,
        descriptor: &OciLayerDescriptor,
        raw_bytes: &[u8],
    ) -> Result<[u8; 32], OciCasError> {
        descriptor.verify_bytes(raw_bytes)?;
        let final_path = self.blob_path(&descriptor.sha256_digest);
        if final_path.exists() {
            let existing = fs::read(&final_path)?;
            descriptor.verify_bytes(&existing)?;
            return Ok(descriptor.sha256_digest);
        }
        atomic_write(&final_path, raw_bytes)?;
        Ok(descriptor.sha256_digest)
    }

    /// Reads and cryptographically re-verifies a stored blob against `descriptor`.
    ///
    /// # Errors
    ///
    /// Fails closed if the blob is missing on disk or its stored bytes were
    /// truncated or tampered with after ingestion.
    pub fn read_verified_blob(
        &self,
        descriptor: &OciLayerDescriptor,
    ) -> Result<Vec<u8>, OciCasError> {
        validate_media_type(&descriptor.media_type)?;
        let path = self.blob_path(&descriptor.sha256_digest);
        if !path.is_file() {
            return Err(OciCasError::BlobMissing {
                digest: descriptor.sha256_digest,
            });
        }
        let bytes = fs::read(&path)?;
        descriptor.verify_bytes(&bytes)?;
        Ok(bytes)
    }

    /// Verifies and writes a complete multi-layer [`OciPackageLayout`] (config
    /// blob, all layer blobs, and canonical manifest record) into local `CAS`.
    ///
    /// # Errors
    ///
    /// Fails closed without writing a manifest record if any layer fails
    /// `SHA-256` or byte-size verification.
    pub fn write_package_layout(
        &self,
        config_descriptor: &OciLayerDescriptor,
        config_bytes: &[u8],
        layers: &[(OciLayerDescriptor, &[u8])],
    ) -> Result<OciPackageLayout, OciCasError> {
        let layout =
            OciPackageLayout::from_verified_streams(config_descriptor, config_bytes, layers)?;
        self.put_layer_blob(&layout.config, config_bytes)?;
        for (descriptor, payload) in layers {
            self.put_layer_blob(descriptor, payload)?;
        }
        let serialized = encode_manifest_record(&layout);
        let manifest_path = self.manifest_path(&layout.oci_manifest_digest);
        if manifest_path.exists() {
            let existing = self.verify_stored_layout(&layout.oci_manifest_digest)?;
            if existing != layout {
                return Err(OciCasError::ManifestCorrupt(
                    "existing manifest content differs for identical SHA-256 address",
                ));
            }
            return Ok(layout);
        }
        atomic_write(&manifest_path, serialized.as_bytes())?;
        Ok(layout)
    }

    /// Re-reads a stored manifest by `expected_oci_manifest_digest`, verifies
    /// every referenced blob on disk (`config` + all `layers`) against its
    /// declared size and `SHA-256` digest, and verifies the recomputed
    /// `oci_manifest_digest` bitwise.
    ///
    /// # Errors
    ///
    /// Fails closed if the manifest or any layer blob is missing, corrupted, or
    /// tampered with on disk.
    pub fn verify_stored_layout(
        &self,
        expected_oci_manifest_digest: &[u8; 32],
    ) -> Result<OciPackageLayout, OciCasError> {
        let path = self.manifest_path(expected_oci_manifest_digest);
        if !path.is_file() {
            return Err(OciCasError::ManifestMissing {
                digest: *expected_oci_manifest_digest,
            });
        }
        let raw_text = fs::read_to_string(&path)?;
        let layout = decode_manifest_record(&raw_text)?;
        if &layout.oci_manifest_digest != expected_oci_manifest_digest {
            return Err(OciCasError::ManifestDigestMismatch {
                expected: *expected_oci_manifest_digest,
                computed: layout.oci_manifest_digest,
            });
        }
        let _ = self.read_verified_blob(&layout.config)?;
        for layer in &layout.layers {
            let _ = self.read_verified_blob(layer)?;
        }
        Ok(layout)
    }

    /// Returns `true` if a manifest file exists at `manifests/sha256/<hex>`.
    #[must_use]
    pub fn contains_manifest(&self, oci_manifest_digest: &[u8; 32]) -> bool {
        self.manifest_path(oci_manifest_digest).is_file()
    }

    /// Returns the filesystem path of a content-addressed blob in `CAS`.
    #[must_use]
    pub fn blob_path(&self, sha256_digest: &[u8; 32]) -> PathBuf {
        self.cas_root
            .join("blobs")
            .join("sha256")
            .join(hex_32(sha256_digest))
    }

    /// Returns the filesystem path of a content-addressed manifest in `CAS`.
    #[must_use]
    pub fn manifest_path(&self, oci_manifest_digest: &[u8; 32]) -> PathBuf {
        self.cas_root
            .join("manifests")
            .join("sha256")
            .join(hex_32(oci_manifest_digest))
    }
}

/// Computes a raw 32-byte `SHA-256` digest over `bytes`.
#[must_use]
pub fn compute_sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn validate_media_type(media_type: &str) -> Result<(), OciCasError> {
    if media_type.is_empty()
        || media_type.len() > 256
        || !media_type.contains('/')
        || media_type.bytes().any(|byte| byte <= b' ' || byte >= 0x7f)
    {
        return Err(OciCasError::InvalidMediaType(media_type.to_owned()));
    }
    Ok(())
}

fn atomic_write(target: &Path, bytes: &[u8]) -> Result<(), OciCasError> {
    let nonce = TEMP_NONCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("cas-entry");
    let tmp_path = target.with_file_name(format!(".{file_name}.{pid}.{nonce}.tmp"));
    fs::write(&tmp_path, bytes)?;
    if let Err(rename_err) = fs::rename(&tmp_path, target) {
        let _ = fs::remove_file(&tmp_path);
        if target.exists() {
            return Ok(());
        }
        return Err(OciCasError::Io(rename_err));
    }
    Ok(())
}

fn encode_manifest_record(layout: &OciPackageLayout) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    out.push_str(OCI_MANIFEST_RECORD_HEADER);
    out.push('\n');
    let _ = writeln!(out, "schema_version {}", layout.schema_version);
    let _ = writeln!(
        out,
        "config {} {} {}",
        layout.config.media_type,
        hex_32(&layout.config.sha256_digest),
        layout.config.size_bytes
    );
    for layer in &layout.layers {
        let _ = writeln!(
            out,
            "layer {} {} {}",
            layer.media_type,
            hex_32(&layer.sha256_digest),
            layer.size_bytes
        );
    }
    out
}

fn decode_manifest_record(text: &str) -> Result<OciPackageLayout, OciCasError> {
    let mut lines = text.lines();
    let header = lines
        .next()
        .ok_or(OciCasError::ManifestCorrupt("empty manifest file"))?;
    if header != OCI_MANIFEST_RECORD_HEADER {
        return Err(OciCasError::ManifestCorrupt("unexpected manifest header"));
    }
    let schema_line = lines
        .next()
        .ok_or(OciCasError::ManifestCorrupt("missing schema_version line"))?;
    let schema_str = schema_line
        .strip_prefix("schema_version ")
        .ok_or(OciCasError::ManifestCorrupt("malformed schema_version"))?;
    let schema_version: u32 = schema_str
        .parse()
        .map_err(|_| OciCasError::ManifestCorrupt("invalid schema_version integer"))?;
    if schema_version != OCI_IMAGE_MANIFEST_SCHEMA_V2 {
        return Err(OciCasError::ManifestCorrupt("unsupported schema_version"));
    }
    let config_line = lines
        .next()
        .ok_or(OciCasError::ManifestCorrupt("missing config descriptor"))?;
    let config = parse_descriptor_line(config_line, "config ")?;
    let mut layers = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        layers.push(parse_descriptor_line(line, "layer ")?);
    }
    if layers.is_empty() {
        return Err(OciCasError::EmptyLayers);
    }
    let oci_manifest_digest =
        compute_canonical_oci_manifest_digest(schema_version, &config, &layers);
    Ok(OciPackageLayout {
        schema_version,
        config,
        layers,
        oci_manifest_digest,
    })
}

fn parse_descriptor_line(line: &str, prefix: &str) -> Result<OciLayerDescriptor, OciCasError> {
    let rest = line
        .strip_prefix(prefix)
        .ok_or(OciCasError::ManifestCorrupt("unexpected descriptor prefix"))?;
    let mut parts = rest.split_whitespace();
    let media_type = parts.next().ok_or(OciCasError::ManifestCorrupt(
        "missing descriptor media_type",
    ))?;
    validate_media_type(media_type)?;
    let digest_hex = parts
        .next()
        .ok_or(OciCasError::ManifestCorrupt("missing descriptor digest"))?;
    let sha256_digest = decode_hex_32(digest_hex).ok_or(OciCasError::ManifestCorrupt(
        "invalid descriptor hex digest",
    ))?;
    let size_str = parts
        .next()
        .ok_or(OciCasError::ManifestCorrupt("missing descriptor size"))?;
    if parts.next().is_some() {
        return Err(OciCasError::ManifestCorrupt(
            "trailing tokens in descriptor",
        ));
    }
    let size_bytes: u64 = size_str
        .parse()
        .map_err(|_| OciCasError::ManifestCorrupt("invalid descriptor size_bytes"))?;
    Ok(OciLayerDescriptor {
        media_type: media_type.to_owned(),
        sha256_digest,
        size_bytes,
    })
}

fn decode_hex_32(hex: &str) -> Option<[u8; 32]> {
    let raw = hex.as_bytes();
    if raw.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (idx, slot) in out.iter_mut().enumerate() {
        let hi = decode_nibble(raw[idx * 2])?;
        let lo = decode_nibble(raw[idx * 2 + 1])?;
        *slot = (hi << 4) | lo;
    }
    Some(out)
}

const fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
