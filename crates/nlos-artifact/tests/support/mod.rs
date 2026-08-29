//! Shared fixtures for nlos-artifact integration tests.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ed25519_dalek::{Signer, SigningKey};
use nlos_artifact::{
    ContentDigest, CreateArtifactSpec, PackageEntryRole, PackageManifest, PackageManifestEntry,
    PutRevisionRequest, SignedPackage, package_manifest_message,
};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose};
use nlos_types::{ApplicationId, ArtifactId, IdempotencyKey, PackageId};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

/// Unique temporary store root, removed recursively on drop.
pub struct TestStoreDir {
    root: PathBuf,
}

impl TestStoreDir {
    pub fn new(name: &str) -> Self {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-artifact-test-{name}-{}-{sequence}",
            std::process::id()
        ));
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn artifact_blob(&self, digest: nlos_artifact::ContentDigest) -> PathBuf {
        let hex = digest.to_hex();
        self.root.join("artifacts/blobs").join(&hex[..2]).join(hex)
    }

    pub fn cache_blob(&self, digest: nlos_artifact::ContentDigest) -> PathBuf {
        let hex = digest.to_hex();
        self.root.join("cache/blobs").join(&hex[..2]).join(hex)
    }
}

impl Drop for TestStoreDir {
    fn drop(&mut self) {
        match fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove test store dir: {error}"),
        }
    }
}

pub fn artifact_id(seed: u8) -> ArtifactId {
    ArtifactId::from_bytes([seed; 16])
}

pub fn artifact_spec(seed: u8) -> CreateArtifactSpec {
    CreateArtifactSpec {
        artifact_id: artifact_id(seed),
        idempotency_key: IdempotencyKey::from_bytes([0xa0 + seed; 16]),
        content_type: "application/octet-stream".to_string(),
        application_id: Some(ApplicationId::from_bytes([0xb0 + seed; 16])),
        owner: Some(format!("user-{seed}")),
        created_at_ms: 1_000 + u64::from(seed),
    }
}

pub fn put(artifact: ArtifactId, expected_head: u64, bytes: &[u8]) -> PutRevisionRequest<'_> {
    PutRevisionRequest {
        artifact_id: artifact,
        expected_head_revision: expected_head,
        bytes,
        created_at_ms: 5_000 + expected_head,
    }
}

pub fn bytes(tag: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|index| tag ^ u8::try_from(index % 251).expect("mod 251 fits in u8"))
        .collect()
}

/// A freshly bootstrapped identity authority with one `SemanticSigning`
/// principal whose key is valid on `[0, 10_000)` ms, mirroring the
/// `nlos-identity` test bootstrap.
pub struct TestIdentity {
    pub authority: IdentityAuthority,
    pub key: SigningKey,
    pub binding: IdentityBinding,
    _dir: TestStoreDir,
}

pub fn test_identity(name: &str, seed: u8) -> TestIdentity {
    let dir = TestStoreDir::new(name);
    let authority = IdentityAuthority::open(dir.root()).expect("open identity authority");
    let key = SigningKey::from_bytes(&[seed; 32]);
    let binding = authority
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(1); 32],
            control_domain_policy_digest: [seed.wrapping_add(2); 32],
            public_key: key.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 0,
            key_valid_until_ms: 10_000,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
            created_at_ms: 0,
        })
        .expect("bootstrap principal")
        .binding();
    TestIdentity {
        authority,
        key,
        binding,
        _dir: dir,
    }
}

pub fn manifest(
    package_seed: u8,
    version: u64,
    entries: Vec<PackageManifestEntry>,
) -> PackageManifest {
    PackageManifest {
        package_id: PackageId::from_bytes([package_seed; 16]),
        version,
        entries,
    }
}

pub fn entry(
    name: &str,
    artifact_id: ArtifactId,
    digest: ContentDigest,
    role: PackageEntryRole,
) -> PackageManifestEntry {
    PackageManifestEntry {
        name: name.to_string(),
        artifact_id,
        digest,
        role,
    }
}

pub fn sign_package(identity: &TestIdentity, manifest: PackageManifest) -> SignedPackage {
    let digest = package_manifest_message(&manifest);
    SignedPackage {
        manifest,
        signer: identity.binding.principal_id,
        signature: identity.key.sign(&digest).to_bytes(),
    }
}

pub fn publish_artifact(
    store: &nlos_artifact::ArtifactStore,
    seed: u8,
    payload: &[u8],
) -> (ArtifactId, ContentDigest) {
    let spec = artifact_spec(seed);
    store
        .create_artifact(spec.clone())
        .expect("create artifact");
    store
        .put_revision(put(spec.artifact_id, 0, payload))
        .expect("put revision");
    (spec.artifact_id, ContentDigest::of_bytes(payload))
}
