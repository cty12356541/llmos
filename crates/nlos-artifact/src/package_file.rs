//! `nlos/package-file/v1` package-file codec (W33-A, lifted from the
//! `nlos-package` bin in W33-C).
//!
//! One authority for the self-contained signed package file format: the
//! framing (magic → signer descriptor → package id/version → entries →
//! task templates → Ed25519 signature, canonical u64 BE length prefixes)
//! is decoded here and consumed by the `nlos-package` bin (build/verify)
//! and the conformance kit ([`crate::conformance`]). Decode failures are
//! typed ([`PackageFileError`]) so callers can classify *which* structure
//! invariant broke instead of parsing message strings.
//!
//! Determinism: no wall-clock input exists anywhere in the format; the
//! documented entry `artifact_id` derivation
//! ([`derive_artifact_id`]) is pure SHA-256 over
//! `(domain ‖ package_id ‖ version ‖ name-length ‖ name)`.

use std::fmt;

use nlos_identity::BootstrapPrincipalRequest;
use nlos_types::{ArtifactId, IdempotencyKey, PackageId};
use sha2::{Digest, Sha256};

use crate::model::ContentDigest;
use crate::package::{
    PackageEntryRole, PackageManifest, PackageManifestEntry, PackageTaskKind, PackageTaskTemplate,
};

/// Magic prefix of every package file.
pub const PACKAGE_FILE_MAGIC: &[u8] = b"nlos/package-file/v1";

/// Entry-name bound of the package-file format and the developer
/// manifest; mirrors the artifact authority's
/// `MAX_TEXT_COMPONENT_BYTES`.
pub const MAX_ENTRY_NAME_BYTES: usize = 255;

/// Domain separator for the documented deterministic entry artifact-id
/// derivation (build reproducibility: identical trees sign to identical
/// package files).
pub const ARTIFACT_ID_DOMAIN: &[u8] = b"llmos/package-file/artifact-id/v1";

/// Typed package-file decode failure. Every variant names the exact
/// structure invariant that broke; the conformance kit maps each to a
/// rule id, and the CLI renders each as a malformed-input (exit 2)
/// failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PackageFileError {
    /// The file does not start with [`PACKAGE_FILE_MAGIC`].
    BadMagic,
    /// A field read ran past the end of the file.
    Truncated { at: &'static str },
    /// Bytes remain after the trailing signature field.
    TrailingBytes { count: usize },
    /// A u64 count does not fit the platform `usize`.
    CountOverflow { what: &'static str },
    /// An entry-name length is zero or exceeds [`MAX_ENTRY_NAME_BYTES`].
    EntryNameLength { len: usize },
    /// An entry name is not UTF-8.
    EntryNameNotUtf8,
    /// An entry name contains a NUL byte.
    EntryNameNul,
    /// An entry role byte outside `1..=3`.
    UnknownEntryRoleByte(u8),
    /// A task-template kind byte outside `1..=2`.
    UnknownTaskKindByte(u8),
    /// The signer key validity window is empty (`from > until`).
    EmptyKeyValidityWindow,
}

impl fmt::Display for PackageFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic => write!(formatter, "bad magic"),
            Self::Truncated { at } => write!(formatter, "truncated package file at {at}"),
            Self::TrailingBytes { count } => {
                write!(formatter, "trailing bytes after signature: {count}")
            }
            Self::CountOverflow { what } => {
                write!(formatter, "{what} count exceeds platform usize")
            }
            Self::EntryNameLength { .. } => {
                write!(formatter, "entry name length out of bounds")
            }
            Self::EntryNameNotUtf8 => write!(formatter, "entry name is not UTF-8"),
            Self::EntryNameNul => write!(formatter, "entry name contains NUL"),
            Self::UnknownEntryRoleByte(byte) => {
                write!(formatter, "unknown entry role byte {byte}")
            }
            Self::UnknownTaskKindByte(byte) => {
                write!(formatter, "unknown task kind byte {byte}")
            }
            Self::EmptyKeyValidityWindow => {
                write!(formatter, "signer key validity window is empty")
            }
        }
    }
}

impl std::error::Error for PackageFileError {}

/// Public signer descriptor embedded in every package file: the exact
/// material `nlos-identity` needs to bootstrap (or replay) the signer
/// principal at verify time. No secret travels with the package.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignerDescriptor {
    pub public_key: [u8; 32],
    pub profile_digest: [u8; 32],
    pub policy_digest: [u8; 32],
    pub bootstrap_key: [u8; 16],
    pub valid_from_ms: u64,
    pub valid_until_ms: u64,
}

impl SignerDescriptor {
    #[must_use]
    pub fn bootstrap_request(&self) -> BootstrapPrincipalRequest {
        BootstrapPrincipalRequest {
            principal_profile_digest: self.profile_digest,
            control_domain_policy_digest: self.policy_digest,
            public_key: self.public_key,
            key_purpose: nlos_identity::KeyPurpose::SemanticSigning,
            key_valid_from_ms: self.valid_from_ms,
            key_valid_until_ms: self.valid_until_ms,
            idempotency_key: IdempotencyKey::from_bytes(self.bootstrap_key),
            created_at_ms: self.valid_from_ms,
        }
    }
}

/// One embedded package entry: manifest declaration fields plus the
/// payload bytes themselves (the file is self-contained).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageFileEntry {
    pub name: String,
    pub role: PackageEntryRole,
    pub artifact_id: [u8; 16],
    pub digest: [u8; 32],
    pub payload: Vec<u8>,
}

impl PackageFileEntry {
    /// The manifest declaration of this entry (name, artifact id,
    /// digest, role) — the fields the signer's signature covers.
    #[must_use]
    pub fn manifest_entry(&self) -> PackageManifestEntry {
        PackageManifestEntry {
            name: self.name.clone(),
            artifact_id: ArtifactId::from_bytes(self.artifact_id),
            digest: ContentDigest::from_bytes(self.digest),
            role: self.role,
        }
    }
}

/// One decoded package file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageFile {
    pub descriptor: SignerDescriptor,
    pub package_id: PackageId,
    pub version: u64,
    pub entries: Vec<PackageFileEntry>,
    pub tasks: Vec<PackageTaskTemplate>,
    pub signature: [u8; 64],
}

impl PackageFile {
    /// The base [`PackageManifest`] this file carries (package identity,
    /// version, and every entry's declared binding).
    #[must_use]
    pub fn manifest(&self) -> PackageManifest {
        PackageManifest {
            package_id: self.package_id,
            version: self.version,
            entries: self
                .entries
                .iter()
                .map(PackageFileEntry::manifest_entry)
                .collect(),
        }
    }

    /// Canonical length-prefixed framing (u64 BE fields), exactly as
    /// `build` writes it.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(PACKAGE_FILE_MAGIC);
        bytes.extend_from_slice(&self.descriptor.public_key);
        bytes.extend_from_slice(&self.descriptor.profile_digest);
        bytes.extend_from_slice(&self.descriptor.policy_digest);
        bytes.extend_from_slice(&self.descriptor.bootstrap_key);
        bytes.extend_from_slice(&self.descriptor.valid_from_ms.to_be_bytes());
        bytes.extend_from_slice(&self.descriptor.valid_until_ms.to_be_bytes());
        bytes.extend_from_slice(self.package_id.as_bytes());
        bytes.extend_from_slice(&self.version.to_be_bytes());
        extend_count(&mut bytes, self.entries.len());
        for entry in &self.entries {
            extend_count(&mut bytes, entry.name.len());
            bytes.extend_from_slice(entry.name.as_bytes());
            bytes.push(entry.role.encode());
            bytes.extend_from_slice(&entry.artifact_id);
            bytes.extend_from_slice(&entry.digest);
            extend_count(&mut bytes, entry.payload.len());
            bytes.extend_from_slice(&entry.payload);
        }
        extend_count(&mut bytes, self.tasks.len());
        for task in &self.tasks {
            bytes.extend_from_slice(&task.node_key);
            bytes.push(task.kind.encode());
            bytes.extend_from_slice(&task.binding_digest);
            extend_count(&mut bytes, task.dependency_keys.len());
            for dependency in &task.dependency_keys {
                bytes.extend_from_slice(dependency);
            }
            bytes.extend_from_slice(&task.input_selectors_digest);
            bytes.extend_from_slice(&task.output_contract_digest);
            bytes.extend_from_slice(&task.policy_digest);
            bytes.extend_from_slice(&task.resource_ceiling_digest);
        }
        bytes.extend_from_slice(&self.signature);
        bytes
    }
}

fn extend_count(bytes: &mut Vec<u8>, count: usize) {
    bytes.extend_from_slice(&u64::try_from(count).unwrap_or(u64::MAX).to_be_bytes());
}

struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, len: usize, what: &'static str) -> Result<&'a [u8], PackageFileError> {
        let end = self
            .position
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(PackageFileError::Truncated { at: what })?;
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn take_array<const N: usize>(
        &mut self,
        what: &'static str,
    ) -> Result<[u8; N], PackageFileError> {
        let mut array = [0_u8; N];
        array.copy_from_slice(self.take(N, what)?);
        Ok(array)
    }

    fn take_count(&mut self, what: &'static str) -> Result<usize, PackageFileError> {
        let raw = u64::from_be_bytes(self.take_array::<8>(what)?);
        usize::try_from(raw).map_err(|_| PackageFileError::CountOverflow { what })
    }

    fn finish(self) -> Result<(), PackageFileError> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(PackageFileError::TrailingBytes {
                count: self.bytes.len() - self.position,
            })
        }
    }
}

/// Decodes one package file, fail-closed on every framing violation.
///
/// # Errors
///
/// Typed [`PackageFileError`]; no partial package is ever returned.
pub fn decode_package_file(bytes: &[u8]) -> Result<PackageFile, PackageFileError> {
    let mut decoder = Decoder::new(bytes);
    let magic = decoder.take(PACKAGE_FILE_MAGIC.len(), "magic")?;
    if magic != PACKAGE_FILE_MAGIC {
        return Err(PackageFileError::BadMagic);
    }
    let descriptor = SignerDescriptor {
        public_key: decoder.take_array("signer public key")?,
        profile_digest: decoder.take_array("profile digest")?,
        policy_digest: decoder.take_array("policy digest")?,
        bootstrap_key: decoder.take_array("bootstrap key")?,
        valid_from_ms: u64::from_be_bytes(decoder.take_array("key valid-from")?),
        valid_until_ms: u64::from_be_bytes(decoder.take_array("key valid-until")?),
    };
    if descriptor.valid_from_ms > descriptor.valid_until_ms {
        return Err(PackageFileError::EmptyKeyValidityWindow);
    }
    let package_id = PackageId::from_bytes(decoder.take_array("package id")?);
    let version = u64::from_be_bytes(decoder.take_array("version")?);

    let entry_count = decoder.take_count("entry count")?;
    let mut entries = Vec::new();
    for _ in 0..entry_count {
        let name_len = decoder.take_count("entry name length")?;
        if name_len == 0 || name_len > MAX_ENTRY_NAME_BYTES {
            return Err(PackageFileError::EntryNameLength { len: name_len });
        }
        let name_bytes = decoder.take(name_len, "entry name")?;
        let name = String::from_utf8(name_bytes.to_vec())
            .map_err(|_| PackageFileError::EntryNameNotUtf8)?;
        if name.contains('\0') {
            return Err(PackageFileError::EntryNameNul);
        }
        let role_byte = decoder.take_array::<1>("entry role")?[0];
        let role = match role_byte {
            1 => PackageEntryRole::Executable,
            2 => PackageEntryRole::BackgroundService,
            3 => PackageEntryRole::Data,
            other => return Err(PackageFileError::UnknownEntryRoleByte(other)),
        };
        let artifact_id = decoder.take_array("entry artifact id")?;
        let digest = decoder.take_array("entry digest")?;
        let payload_len = decoder.take_count("entry payload length")?;
        let payload = decoder.take(payload_len, "entry payload")?.to_vec();
        entries.push(PackageFileEntry {
            name,
            role,
            artifact_id,
            digest,
            payload,
        });
    }

    let task_count = decoder.take_count("task count")?;
    let mut tasks = Vec::new();
    for _ in 0..task_count {
        let node_key = decoder.take_array("task node key")?;
        let kind_byte = decoder.take_array::<1>("task kind")?[0];
        let kind = match kind_byte {
            1 => PackageTaskKind::AgentRole,
            2 => PackageTaskKind::Executable,
            other => return Err(PackageFileError::UnknownTaskKindByte(other)),
        };
        let binding_digest = decoder.take_array("task binding")?;
        let dependency_count = decoder.take_count("task dependency count")?;
        let mut dependency_keys = Vec::with_capacity(dependency_count);
        for _ in 0..dependency_count {
            dependency_keys.push(decoder.take_array("task dependency")?);
        }
        tasks.push(PackageTaskTemplate {
            node_key,
            kind,
            binding_digest,
            dependency_keys,
            input_selectors_digest: decoder.take_array("task inputs")?,
            output_contract_digest: decoder.take_array("task outputs")?,
            policy_digest: decoder.take_array("task policy")?,
            resource_ceiling_digest: decoder.take_array("task ceiling")?,
        });
    }

    let signature = decoder.take_array("signature")?;
    decoder.finish()?;
    Ok(PackageFile {
        descriptor,
        package_id,
        version,
        entries,
        tasks,
        signature,
    })
}

/// The documented deterministic entry artifact-id derivation:
/// `SHA-256(ARTIFACT_ID_DOMAIN ‖ package_id ‖ version ‖ name-length ‖
/// name)`, truncated to 16 bytes. The name length participates, so name
/// boundaries cannot collide.
#[must_use]
pub fn derive_artifact_id(package_id: PackageId, version: u64, name: &str) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(ARTIFACT_ID_DOMAIN);
    hasher.update(package_id.as_bytes());
    hasher.update(version.to_be_bytes());
    hasher.update(u64::try_from(name.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(name.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_ENTRY_NAME_BYTES, PACKAGE_FILE_MAGIC, PackageEntryRole, PackageFile, PackageFileEntry,
        PackageFileError, PackageTaskKind, PackageTaskTemplate, SignerDescriptor,
        decode_package_file, derive_artifact_id,
    };
    use ed25519_dalek::SigningKey;
    use nlos_types::PackageId;
    use sha2::{Digest, Sha256};

    fn descriptor(seed: u8) -> SignerDescriptor {
        SignerDescriptor {
            public_key: SigningKey::from_bytes(&[seed; 32])
                .verifying_key()
                .to_bytes(),
            profile_digest: [seed.wrapping_add(1); 32],
            policy_digest: [seed.wrapping_add(2); 32],
            bootstrap_key: [seed.wrapping_add(3); 16],
            valid_from_ms: 0,
            valid_until_ms: u64::try_from(i64::MAX).expect("i64::MAX fits u64"),
        }
    }

    fn entry(name: &str, body: &[u8]) -> PackageFileEntry {
        let mut digest = [0_u8; 32];
        let mut hasher = Sha256::new();
        hasher.update(body);
        digest.copy_from_slice(&hasher.finalize());
        PackageFileEntry {
            name: name.to_string(),
            role: PackageEntryRole::Executable,
            artifact_id: derive_artifact_id(
                PackageId::from_bytes([0xab; 16]),
                1 << 32 | 2 << 16 | 3,
                name,
            ),
            digest,
            payload: body.to_vec(),
        }
    }

    fn package_file(with_tasks: bool) -> PackageFile {
        let tasks = if with_tasks {
            vec![PackageTaskTemplate {
                node_key: [0x33; 16],
                kind: PackageTaskKind::AgentRole,
                binding_digest: [0x44; 32],
                dependency_keys: vec![[0x55; 16]],
                input_selectors_digest: [0x66; 32],
                output_contract_digest: [0x77; 32],
                policy_digest: [0x88; 32],
                resource_ceiling_digest: [0x99; 32],
            }]
        } else {
            Vec::new()
        };
        PackageFile {
            descriptor: descriptor(0x5a),
            package_id: PackageId::from_bytes([0xab; 16]),
            version: 1 << 32 | 2 << 16 | 3,
            entries: vec![
                entry("hello", b"payload-hello"),
                entry("config", b"payload-config"),
            ],
            tasks,
            signature: [0xcd; 64],
        }
    }

    #[test]
    fn codec_round_trips_both_faces() {
        for with_tasks in [false, true] {
            let package = package_file(with_tasks);
            let encoded = package.encode();
            assert_eq!(&encoded[..PACKAGE_FILE_MAGIC.len()], PACKAGE_FILE_MAGIC);
            let decoded = decode_package_file(&encoded).expect("decode");
            assert_eq!(decoded, package);
        }
    }

    #[test]
    fn codec_failures_are_typed() {
        let encoded = package_file(false).encode();
        assert_eq!(
            decode_package_file(&encoded[..encoded.len() - 1]),
            Err(PackageFileError::Truncated { at: "signature" })
        );
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_eq!(
            decode_package_file(&trailing),
            Err(PackageFileError::TrailingBytes { count: 1 })
        );
        let mut bad_magic = encoded.clone();
        bad_magic[0] ^= 0xff;
        assert_eq!(
            decode_package_file(&bad_magic),
            Err(PackageFileError::BadMagic)
        );

        // Reversed key window fails with the typed window error.
        let mut reversed = package_file(false);
        reversed.descriptor.valid_from_ms = 9;
        reversed.descriptor.valid_until_ms = 1;
        assert_eq!(
            decode_package_file(&reversed.encode()),
            Err(PackageFileError::EmptyKeyValidityWindow)
        );

        // Oversized and NUL-bearing names are typed shape failures.
        let mut oversized = package_file(false);
        oversized.entries[0].name = "x".repeat(MAX_ENTRY_NAME_BYTES + 1);
        assert!(matches!(
            decode_package_file(&oversized.encode()),
            Err(PackageFileError::EntryNameLength { len })
            if len == MAX_ENTRY_NAME_BYTES + 1
        ));
        let mut nul_bearing = package_file(false);
        nul_bearing.entries[0].name = "he\0lo".to_string();
        assert_eq!(
            decode_package_file(&nul_bearing.encode()),
            Err(PackageFileError::EntryNameNul)
        );

        // Unknown enum bytes are typed; surgery: the role byte follows
        // the (unique) entry name in the framing.
        let mut unknown_role = encoded;
        let position = unknown_role
            .windows(5)
            .position(|window| window == b"hello")
            .expect("name needle")
            + 5;
        unknown_role[position] = 7;
        assert_eq!(
            decode_package_file(&unknown_role),
            Err(PackageFileError::UnknownEntryRoleByte(7))
        );
    }

    #[test]
    fn artifact_id_is_deterministic_and_name_bounded() {
        let package_id = PackageId::from_bytes([9; 16]);
        let first = derive_artifact_id(package_id, 1, "alpha");
        assert_eq!(first, derive_artifact_id(package_id, 1, "alpha"));
        assert_ne!(first, derive_artifact_id(package_id, 2, "alpha"));
        assert_ne!(
            first,
            derive_artifact_id(package_id, 1, &"x".repeat(MAX_ENTRY_NAME_BYTES))
        );
        // Length prefix participates: a name boundary shift moves the id.
        assert_ne!(
            derive_artifact_id(package_id, 1, "ab"),
            derive_artifact_id(package_id, 1, "a")
        );
    }

    #[test]
    fn manifest_projection_carries_every_signed_field() {
        let package = package_file(false);
        let manifest = package.manifest();
        assert_eq!(manifest.package_id, package.package_id);
        assert_eq!(manifest.version, package.version);
        assert_eq!(manifest.entries.len(), package.entries.len());
        assert_eq!(manifest.entries[0].name, package.entries[0].name);
        assert_eq!(
            manifest.entries[0].artifact_id.as_bytes(),
            &package.entries[0].artifact_id
        );
        assert_eq!(
            manifest.entries[0].digest.as_bytes(),
            &package.entries[0].digest[..]
        );
    }
}
