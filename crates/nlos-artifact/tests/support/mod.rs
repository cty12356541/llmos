//! Shared fixtures for nlos-artifact integration tests.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_artifact::{CreateArtifactSpec, PutRevisionRequest};
use nlos_types::{ApplicationId, ArtifactId, IdempotencyKey};

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
