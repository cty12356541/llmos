//! Content-addressed artifact store with `SQLite` metadata
//! (B-ARTIFACT-001/002).
//!
//! Per the stage-B technology selection (§7) and v0.5 §14:
//!
//! ```text
//! ArtifactId + revision + ContentDigest -> SQLite metadata (WAL/FULL)
//! ContentDigest                         -> local content-addressed bytes
//! ```
//!
//! # Layout
//!
//! ```text
//! <root>/metadata.db                    artifact/revision/cache metadata
//! <root>/artifacts/blobs/<2-hex>/<digest>   artifact本体 blobs (immutable)
//! <root>/artifacts/tmp/                 pre-rename scratch (same device)
//! <root>/cache/blobs/<2-hex>/<digest>   evictable derived cache blobs
//! <root>/cache/tmp/
//! ```
//!
//! `artifacts/` and `cache/` are separate retention domains
//! (`[CTX-NOTDATA-001]`): cache eviction never touches artifact blobs.
//!
//! # Commit protocol and crash windows
//!
//! [`ArtifactStore::put_revision`] commits in two ordered phases:
//!
//! 1. **Blob**: tmp write → fsync file → re-read + digest verify → atomic
//!    rename → fsync parent directory (see `blob.rs`).
//! 2. **Metadata**: one `BEGIN IMMEDIATE` transaction inserts the immutable
//!    revision row and compare-and-swaps the head pointer.
//!
//! Blob durability always precedes the metadata commit that references the
//! digest. A crash before the rename leaves an orphan tmp file (removed by
//! [`ArtifactStore::recover`]); after the rename but before the metadata
//! commit leaves an orphan blob (listed by `recover`, never deleted here);
//! after the metadata commit the revision is fully usable.
//!
//! [`ArtifactStore::stage_revision`] uses the same durable blob phase but
//! writes only staged metadata: it never creates a revision or advances the
//! canonical head. [`ArtifactStore::publish_staged_revision`] later verifies
//! the task/permit/write-set binding and atomically inserts the immutable
//! revision, compare-and-swaps head, writes an immutable publication receipt,
//! and marks the stage published in one `SQLite` transaction.
//!
//! # Package verification prefix (B-ARTIFACT-003)
//!
//! [`ArtifactStore::verify_package`] verifies a minimal signed Package
//! envelope: the acting principal's Ed25519 signature over the
//! domain-separated manifest digest (checked by `nlos-identity` under the
//! signer's *current* key binding) and every manifest entry's content
//! binding against the artifact heads. Success commits one immutable
//! package verification receipt; replays are durable-authoritative and
//! never re-verify. See `package` for the exact fail-closed order and the
//! scope boundaries.
//!
//! # Scope and honesty boundaries
//!
//! - `DeploymentMode=LOCAL_SINGLE_NODE` only (`[ART-LOCAL-001]`): the local
//!   store is authoritative; no sync/distributed/object-store backend. The
//!   blob layer is confined to the internal `blob` module so a later slice
//!   can lift it behind a backend trait.
//! - No GC execution, retention policy, encryption, provenance chains, or
//!   legal hold. Package verification is a minimal prefix only: no
//!   installation/update lifecycle, no full §23.2 manifest, no trust-root
//!   or signature-chain policy (single signing principal), and no
//!   cross-process verification of the envelope.
//! - [`ArtifactStore::recover`] is **explicit**, not run on open: open
//!   latency stays predictable and recovery reporting is an operator
//!   decision. Callers may invoke it immediately after open.

mod blob;
mod cache;
mod model;
mod package;
mod publication;
mod query;
mod recover;
mod schema;
mod store;

use std::error::Error;
use std::fmt;
use std::io;
use std::path::PathBuf;

use nlos_identity::IdentityAuthorityError;
use nlos_types::{ArtifactId, PrincipalId};

pub use model::{
    ArtifactHeadEndpointProof, ArtifactPublicationReceipt, ArtifactRecord, ContentDigest,
    CreateArtifactDecision, CreateArtifactSpec, HeadState, MissingBlob, MissingStagedBlob,
    PublishStagedRevisionDecision, PublishStagedRevisionRequest, PutRevisionDecision,
    PutRevisionRequest, RecoveryReport, RevisionRecord, StageRevisionDecision,
    StageRevisionRequest, StagedRevisionRecord, StagedRevisionState, StagingId,
};
pub use package::{
    PackageEntryRole, PackageManifest, PackageManifestEntry, PackageVerificationDecision,
    PackageVerificationReceipt, SignedPackage, VerifyPackageRequest, package_manifest_message,
};
pub use publication::staging_id_for;
pub use store::ArtifactStore;

/// Typed errors of the artifact store. No `anyhow`; every failure mode a
/// caller can act on is its own variant.
#[derive(Debug)]
pub enum ArtifactError {
    /// `SQLite` authority failure (see source for the original error).
    Sqlite(rusqlite::Error),
    /// Filesystem failure outside the classified variants below.
    Io(io::Error),
    /// WAL/FULL durability could not be established (pragmas read back).
    DurabilityUnavailable {
        journal_mode: String,
        synchronous: i64,
    },
    /// Stored `user_version` is newer than or otherwise unknown to this
    /// build; fail closed rather than guess.
    SchemaVersionUnsupported(i64),
    /// No artifact with this identity exists.
    ArtifactNotFound(ArtifactId),
    /// No staged revision with this authority-derived identity exists.
    StagedRevisionNotFound(StagingId),
    /// No immutable publication receipt with this identity exists.
    PublicationReceiptNotFound(nlos_types::ReceiptId),
    /// The artifact exists but has no such revision.
    RevisionNotFound {
        artifact_id: ArtifactId,
        revision: u64,
    },
    /// The idempotency key (or artifact identity) was reused with a
    /// different specification.
    IdempotencyConflict,
    /// A publish request does not reproduce the task, permit, and write-set
    /// binding stored with the staged revision.
    PublicationBindingMismatch,
    /// The observed head does not match `expected_head_revision`: either a
    /// competing put advanced the head first, or the expectation names a
    /// revision that does not exist yet. Fail closed; re-resolve the head
    /// and retry.
    HeadConflict { expected: u64, current: u64 },
    /// An immutable revision slot is occupied by different content while the
    /// head claims the slot is free. Normally unreachable through
    /// [`ArtifactStore::put_revision`]; the fail-closed guard for metadata
    /// inconsistency (`[ART-VERSION-001]` immutability spirit).
    RevisionConflict {
        artifact_id: ArtifactId,
        revision: u64,
    },
    /// A committed revision's blob is absent from the local store. Run
    /// [`ArtifactStore::recover`] to reconcile and report all missing blobs.
    BlobMissing {
        artifact_id: ArtifactId,
        revision: u64,
        digest: ContentDigest,
        path: PathBuf,
    },
    /// A staged revision's blob is absent, so publication cannot proceed.
    StagedBlobMissing {
        staging_id: StagingId,
        digest: ContentDigest,
        path: PathBuf,
    },
    /// Bytes read back (or verified during commit) do not match the
    /// addressed digest. Wrong bytes are never returned silently.
    DigestMismatch {
        expected: ContentDigest,
        actual: ContentDigest,
        path: PathBuf,
    },
    /// `ENOSPC`/`EDQUOT` while writing a blob. No metadata was committed.
    BlobNoSpace,
    /// The tmp directory and the blob directory are on different devices;
    /// the rename-based atomic commit protocol requires one filesystem.
    CrossDeviceRename,
    /// A caller-supplied bounded string was empty, oversized, or contained
    /// a NUL byte.
    InvalidSpec(&'static str),
    /// The package manifest violates its minimal structural contract (empty
    /// entries, invalid or duplicate entry names).
    PackageManifestInvalid(&'static str),
    /// The package signature does not verify over the manifest digest
    /// (ADR-0010 `SignatureInvalid` semantics).
    PackageSignatureInvalid,
    /// The package signer principal does not exist in the identity
    /// authority (ADR-0010 `PrincipalUnknown` semantics).
    PackagePrincipalUnknown(PrincipalId),
    /// The package signer's current key binding is revoked
    /// (ADR-0010 `KeyRevoked` semantics).
    PackageKeyRevoked,
    /// Another identity-authority failure during package signature
    /// verification (key purpose, validity window, malformed key).
    PackageIdentity(IdentityAuthorityError),
    /// A manifest entry's declared digest does not match the artifact
    /// store's actual content head (`None` = the artifact exists but has no
    /// revisions yet).
    PackageTampered {
        entry: String,
        expected: ContentDigest,
        actual: Option<ContentDigest>,
    },
    /// No immutable package verification receipt with this identity exists.
    PackageVerificationReceiptNotFound(nlos_types::ReceiptId),
    /// A durable row violates an invariant this crate enforces.
    CorruptRecord(&'static str),
    /// The process-local writer mutex is poisoned.
    LockPoisoned,
}

impl fmt::Display for ArtifactError {
    // Every typed failure mode stays visible as one exhaustive match arm
    // here, so readers can audit all diagnostics in one place.
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite metadata failure: {error}"),
            Self::Io(error) => write!(formatter, "blob I/O failure: {error}"),
            Self::DurabilityUnavailable {
                journal_mode,
                synchronous,
            } => write!(
                formatter,
                "WAL/FULL durability unavailable: journal_mode={journal_mode}, synchronous={synchronous}"
            ),
            Self::SchemaVersionUnsupported(version) => {
                write!(formatter, "unsupported artifact schema version {version}")
            }
            Self::ArtifactNotFound(artifact_id) => {
                write!(formatter, "artifact {artifact_id:?} does not exist")
            }
            Self::StagedRevisionNotFound(staging_id) => {
                write!(formatter, "staged revision {staging_id:?} does not exist")
            }
            Self::PublicationReceiptNotFound(receipt_id) => {
                write!(
                    formatter,
                    "publication receipt {receipt_id:?} does not exist"
                )
            }
            Self::RevisionNotFound {
                artifact_id,
                revision,
            } => write!(
                formatter,
                "artifact {artifact_id:?} has no revision {revision}"
            ),
            Self::IdempotencyConflict => formatter.write_str(
                "idempotency key or artifact identity reused for a different specification",
            ),
            Self::PublicationBindingMismatch => formatter.write_str(
                "publication request does not match the staged task/permit/write-set binding",
            ),
            Self::HeadConflict { expected, current } => write!(
                formatter,
                "artifact head conflict: expected {expected}, current {current}"
            ),
            Self::RevisionConflict {
                artifact_id,
                revision,
            } => write!(
                formatter,
                "immutable revision conflict on artifact {artifact_id:?} revision {revision}"
            ),
            Self::BlobMissing {
                artifact_id,
                revision,
                digest,
                path,
            } => write!(
                formatter,
                "blob {digest} for artifact {artifact_id:?} revision {revision} is missing at {}",
                path.display()
            ),
            Self::StagedBlobMissing {
                staging_id,
                digest,
                path,
            } => write!(
                formatter,
                "blob {digest} for staged revision {staging_id:?} is missing at {}",
                path.display()
            ),
            Self::DigestMismatch {
                expected,
                actual,
                path,
            } => write!(
                formatter,
                "digest mismatch at {}: expected {expected}, actual {actual}",
                path.display()
            ),
            Self::BlobNoSpace => formatter.write_str("no space left on device during blob write"),
            Self::CrossDeviceRename => formatter.write_str(
                "tmp and blob directories are on different devices; atomic rename impossible",
            ),
            Self::InvalidSpec(reason) => {
                write!(formatter, "invalid artifact specification: {reason}")
            }
            Self::PackageManifestInvalid(reason) => {
                write!(formatter, "invalid package manifest: {reason}")
            }
            Self::PackageSignatureInvalid => {
                formatter.write_str("package manifest signature is invalid")
            }
            Self::PackagePrincipalUnknown(id) => {
                write!(formatter, "package signer principal {id:?} does not exist")
            }
            Self::PackageKeyRevoked => formatter.write_str("package signer key is revoked"),
            Self::PackageIdentity(error) => {
                write!(formatter, "package identity verification failure: {error}")
            }
            Self::PackageTampered {
                entry,
                expected,
                actual,
            } => write_package_tampered(formatter, entry, expected, *actual),
            Self::PackageVerificationReceiptNotFound(receipt_id) => write!(
                formatter,
                "package verification receipt {receipt_id:?} does not exist"
            ),
            Self::CorruptRecord(reason) => write!(formatter, "corrupt durable record: {reason}"),
            Self::LockPoisoned => formatter.write_str("artifact writer lock is poisoned"),
        }
    }
}

impl Error for ArtifactError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::PackageIdentity(error) => Some(error),
            _ => None,
        }
    }
}

fn write_package_tampered(
    formatter: &mut fmt::Formatter<'_>,
    entry: &str,
    expected: &ContentDigest,
    actual: Option<ContentDigest>,
) -> fmt::Result {
    write!(
        formatter,
        "package entry {entry:?} is tampered: manifest declares digest {expected}, artifact head is "
    )?;
    match actual {
        Some(digest) => write!(formatter, "{digest}"),
        None => formatter.write_str("absent"),
    }
}

impl From<rusqlite::Error> for ArtifactError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
