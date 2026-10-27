//! Federation mechanism face, minimal slice (W50-L2 + W51-L1): cross-Cell
//! name/service discovery, migration-intent registration, and
//! reconciliation/audit checkpoint registration over one shared file
//! directory — the three §26.1 control-plane responsibilities ADR-0019
//! decision 5 names as the mechanism face ("跨 Cell 名称与服务发现",
//! "placement 与 migration intent", "reconciliation 和审计 checkpoint").
//!
//! **Deliberately not here:** cross-Cell atomic commit, global consensus,
//! intent *execution* (that is `C-MIGRATE`), checkpoint *execution*, any
//! cross-Cell reconciliation coordination (ADR-0019 extension point B
//! leaves those shapes to its finalization), and any cross-host claims.
//! (Since `CS0-B` + `CS1-D`, the digest and record-binding *verification
//! primitives* — [`CheckpointDigest`], [`CheckpointFact::verify_digest`],
//! [`DistTaskRecordBinding`], and [`verify_dist_task_binding`] — live here;
//! consuming them inside the reconciliation convergence core and production
//! takeover drive lives in `nlos-slice-k` (`CS0-C` / `CS1-D`).) A shared-root
//! directory is a single-host,
//! ADR-0018-topology mechanism: two OS processes on one host each holding
//! one Cell discover each other through files, nothing more. Citing this
//! face as cross-machine evidence would violate RISK-B-11.
//!
//! **Layout** (one caller-supplied federation root, `std`-only):
//!
//! - `<root>/cells/<hex identity>.cell` — one registration file per Cell.
//!   The owning `CellHost` publishes (or heartbeats) by replacing the file
//!   whole through write-temp + rename, so every reader sees one complete
//!   record, never a torn one; the record's `heartbeat` counter is the file
//!   generation and a publish that would move it backwards is refused.
//! - `<root>/migration-intents/<recorded_ms>-<pid>-<sequence>.intent` —
//!   one immutable file per intent, append-only by construction (the
//!   writer never rewrites or removes); enumeration sorts by
//!   (`recorded_ms`, `pid`, `sequence`).
//! - `<root>/checkpoints/<hex identity>-<recorded_ms>-<pid>-<sequence>.ckpt`
//!   — one immutable checkpoint file per registered fact, append-only by
//!   construction; the owning Cell's identity is the filename prefix, so
//!   enumerating one Cell's audit trail never reads another Cell's files,
//!   and enumeration sorts by (`recorded_ms`, `pid`, `sequence`).
//!
//! Freshness (`live_cells`) compares the record's wall `heartbeat_ms`
//! against a caller-supplied silence window. That is a single-host
//! discovery heuristic, not a fencing oracle: liveness here grants no
//! authority, and `[DIST-LOCAL-001]` still binds what a Cell may act on.
//! A registered checkpoint is likewise a *claim by its author*: the log
//! itself stores and enumerates the fact verbatim, without judging it.
//! Since `CS0-B` the structured half of that claim is pinnable and checkable
//! here: [`CheckpointDigest::from_prefix`] derives a domain-separated
//! `SHA-256` over the fence axes plus a canonical prefix serialization,
//! and [`CheckpointFact::verify_digest`] recomputes and compares it
//! fail-closed. Since `CS1-D`, [`DistTaskRecordBinding`] and
//! [`verify_dist_task_binding`] cryptographically bind [`MigrationObject`],
//! [`MigrationIntent`], and [`CheckpointPrefix`] entries to concrete §26.1
//! `DIST-TASK` records (`TaskAuthorityAssignment`,
//! `TaskAuthorityTakeoverReceipt`, `TaskAuthorityParticipantRegistry`) via
//! the `"llmos/dist-task-migration-binding/v1"` `SHA-256` commitment, while
//! the reconciliation executor (`CS0-C`) and production takeover drive
//! (`CS1-D`) in `nlos-slice-k` consume those verified claims.

use std::fmt::{self, Write as _};
use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_types::Generation;
use sha2::{Digest, Sha256};

use crate::{CellEpoch, CellFencingToken, CellIdentity};

/// Sub-directory holding one registration file per Cell.
const CELLS_DIR: &str = "cells";
/// Extension of a Cell registration file.
const CELL_FILE_EXT: &str = "cell";
/// Sub-directory holding one immutable file per migration intent.
const INTENTS_DIR: &str = "migration-intents";
/// Extension of a migration-intent file.
const INTENT_FILE_EXT: &str = "intent";
/// Sub-directory holding one immutable checkpoint file per registered
/// reconciliation/audit fact.
const CHECKPOINTS_DIR: &str = "checkpoints";
/// Extension of a checkpoint file.
const CHECKPOINT_FILE_EXT: &str = "ckpt";
/// Upper bound of the free-text fields (description / reason / digest),
/// in bytes.
const MAX_TEXT_BYTES: usize = 256;
/// Process-unique sequence feeding intent filenames, so two records from
/// one process can never collide on the same name.
static INTENT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// Process-unique sequence feeding checkpoint filenames, same collision
/// discipline as [`INTENT_SEQUENCE`].
static CHECKPOINT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// Domain-separation string hashed into every structured checkpoint
/// digest — one domain, one meaning, same discipline as the
/// task-authority domains (`"llmos/task-authority-assignment/v1"`).
const CHECKPOINT_DIGEST_DOMAIN: &[u8] = b"llmos/reconciliation-checkpoint/v1";
/// Prefix marking a digest axis as the canonical text form of a
/// [`CheckpointDigest`] (everything else on that axis is author free
/// text). Canonical bodies are exactly 64 lowercase hex characters.
const STRUCTURED_DIGEST_TAG: &str = "v1:sha256:";
/// Domain-separation string hashed into every `DIST-TASK` record migration
/// binding digest (`CS1-D`).
pub const DIST_TASK_BINDING_DOMAIN: &[u8] = b"llmos/dist-task-migration-binding/v1";
/// Fixed 8-byte header marking a [`CheckpointPrefix`] entry as a canonical
/// [`DistTaskRecordBinding`] payload (`CS1-D`).
const DIST_TASK_ENTRY_MAGIC: &[u8; 8] = b"DTBINDV1";
/// Prefix marking a [`MigrationIntent::reason`] axis as the canonical text
/// form of a [`DistTaskRecordBinding::binding_digest`] (`CS1-D`).
const STRUCTURED_INTENT_REASON_TAG: &str = "v1:dist-task-binding:";

/// The shared-root cross-Cell directory: registration files under
/// `<root>/cells/`, the append-only migration-intent log under
/// `<root>/migration-intents/`, and the append-only checkpoint log under
/// `<root>/checkpoints/`.
///
/// One handle per process (the owning `CellHost` holds it); opening is
/// idempotent and safe to race — `create_dir_all` only.
#[derive(Debug)]
pub struct CellDirectory {
    root: PathBuf,
}

/// One Cell's published registration record — the discovery read face:
/// stable identity, fence axes, and the heartbeat freshness pair.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CellRegistrationEntry {
    identity: CellIdentity,
    node_boot_generation: Generation,
    epoch: CellEpoch,
    fencing_token: CellFencingToken,
    os_process_id: u32,
    description: String,
    heartbeat: u64,
    heartbeat_ms: u64,
}

/// Enumeration result of `<root>/cells/`: every parseable registration,
/// sorted by identity, plus how many files failed to parse (fail-closed
/// per file: one corrupt foreign registration does not brick discovery,
/// but it is never silently hidden either).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CellDirectorySnapshot {
    entries: Vec<CellRegistrationEntry>,
    corrupt: usize,
}

/// The 16-byte object reference a migration intent is about.
///
/// On disk, the intent log stores 16 opaque bytes so non-task `C-MIGRATE`
/// callers remain supported; since `CS1-D`, [`DistTaskRecordBinding`]
/// ([`MigrationObject::from_dist_task_binding`] /
/// [`DistTaskRecordBinding::migration_object`]) and
/// [`verify_dist_task_binding`] deterministically project and verify §26.1
/// `DIST-TASK` records against [`MigrationObject`], [`MigrationIntent`], and
/// [`CheckpointFact`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MigrationObject([u8; 16]);

/// Which §26.1 `DIST-TASK` record family a [`DistTaskRecordBinding`]
/// commits to (`CS1-D`).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DistTaskMigrationKind {
    /// §26.1 `TaskAuthorityAssignment` record.
    AuthorityAssignment,
    /// §26.1 `TaskAuthorityTakeoverReceipt` record.
    TakeoverReceipt,
    /// §26.1 `TaskAuthorityParticipantRegistry` record.
    ParticipantRegistry,
}

/// Cryptographic binding descriptor linking an opaque [`MigrationObject`],
/// a [`MigrationIntent`], and a [`CheckpointPrefix`] entry to a concrete
/// §26.1 `DIST-TASK` record (`CS1-D`).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DistTaskRecordBinding {
    /// Which §26.1 `DIST-TASK` record family is bound.
    pub kind: DistTaskMigrationKind,
    /// 16-byte `TaskId`.
    pub task_id: [u8; 16],
    /// Monotonic `Task` generation (`>= 1`).
    pub task_generation: u64,
    /// 16-byte primary record identity (`assignment_id`, `receipt_id`, or
    /// `registry_id`).
    pub record_id: [u8; 16],
    /// Authority lease term bound by the record (`>= 1`).
    pub authority_term: u64,
    /// Task control epoch bound by the record.
    pub control_epoch: u64,
    /// Participant registry generation bound by the record (`>= 1`).
    pub participant_registry_generation: u64,
    /// 32-byte `participant_registry_root`.
    pub participant_registry_root: [u8; 32],
    /// Optional 32-byte `exact_fence_set_root` (`Some` on takeover
    /// receipts with a resolved fence manifest).
    pub exact_fence_set_root: Option<[u8; 32]>,
}

/// Fail-closed verification outcome when checking a [`MigrationIntent`]
/// and/or [`CheckpointFact`] + [`CheckpointPrefix`] against an expected
/// [`DistTaskRecordBinding`] (`CS1-D`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DistTaskBindingVerification {
    /// Cryptographic digest and `DIST-TASK` record binding match
    /// byte-for-byte.
    Verified,
    /// The `MigrationIntent` or `CheckpointPrefix` entry binds a different
    /// `DIST-TASK` record or tampered field values. Fail closed.
    Mismatch {
        /// Expected 32-byte binding commitment.
        expected_digest: [u8; 32],
        /// Presented 32-byte binding commitment (or `None` when only the
        /// 16-byte `MigrationObject` or `Generation` axis differed).
        presented_digest: Option<[u8; 32]>,
    },
    /// The `CheckpointFact` digest failed verification against the claimed
    /// `CheckpointPrefix`. Fail closed.
    CheckpointDigestMismatch {
        /// Digest presented by the `CheckpointFact`.
        presented: CheckpointDigest,
        /// Digest recomputed over the `CheckpointPrefix`.
        recomputed: CheckpointDigest,
    },
    /// Neither structured binding tag nor `DTBINDV1` prefix entry was
    /// present (for example free-text checkpoint or unbound prefix).
    /// Fail closed for takeover binding claims.
    Unbound,
    /// A tagged intent reason, checkpoint digest, or `DTBINDV1` entry
    /// failed to decode or had zero generation/term axes. Fail closed.
    Corrupt,
}

/// A migration intent as its author states it: source Cell, target Cell,
/// object, the object's fenced generation, and a reason digest. Recording
/// an intent executes nothing (`C-MIGRATE` owns execution).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationIntent {
    source: CellIdentity,
    target: CellIdentity,
    object: MigrationObject,
    generation: Generation,
    reason: String,
}

/// One recorded intent: the author's intent plus the log's own ordering
/// axes (`recorded_ms`, `os_process_id`, `sequence`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationIntentRecord {
    intent: MigrationIntent,
    recorded_ms: u64,
    os_process_id: u32,
    sequence: u64,
}

/// Enumeration result of `<root>/migration-intents/`: every parseable
/// record in append order, plus the corrupt-file count.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationIntentLog {
    records: Vec<MigrationIntentRecord>,
    corrupt: usize,
}

/// One Cell's authoritative-side checkpoint fact as its author states it:
/// the fence snapshot (boot generation, epoch, fencing token) the fact is
/// taken at, plus a one-line digest axis — either author free text (for
/// example a high-water summary of the Cell's own authorities) or the
/// canonical text form of a [`CheckpointDigest`]. Registering a fact still
/// verifies nothing: the log stores and replays the claim verbatim;
/// [`Self::verify_digest`] is the separate, explicit check.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointFact {
    node_boot_generation: Generation,
    epoch: CellEpoch,
    fencing_token: CellFencingToken,
    digest: String,
}

/// One registered checkpoint: the owning Cell's identity, the author's
/// fact, and the log's own ordering axes (`recorded_ms`,
/// `os_process_id`, `sequence`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointRecord {
    cell: CellIdentity,
    fact: CheckpointFact,
    recorded_ms: u64,
    os_process_id: u32,
    sequence: u64,
}

/// Enumeration result of one Cell's files under `<root>/checkpoints/`:
/// every parseable record in append order, plus the corrupt-file count
/// (files under the Cell's filename prefix that do not parse, or that
/// parse into a record naming a different Cell — a shape this face never
/// writes).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointLog {
    records: Vec<CheckpointRecord>,
    corrupt: usize,
}

/// The claimed durable prefix a checkpoint digest is taken over: an
/// ordered, append-only sequence of opaque entries. The *meaning* of each
/// entry (a commit id, a row image, an exact root) is fixed by the
/// authority that computes or checks the digest — this face pins only the
/// byte grammar, so the author and any verifier serialize one prefix one
/// way.
#[derive(Clone, Default, Debug, Eq, PartialEq)]
pub struct CheckpointPrefix {
    entries: Vec<Vec<u8>>,
}

/// The structured digest of one checkpoint claim: a domain-separated
/// `SHA-256` pinning a [`CheckpointPrefix`] to the fence snapshot (boot
/// generation, epoch, fencing token) the claim was taken at. Any change
/// to any axis or any prefix byte changes the digest.
///
/// The digest's canonical single-line text form (`v1:sha256:` + 64
/// lowercase hex characters, 74 bytes) fits the fact's `MAX_TEXT_BYTES`
/// digest axis unchanged, so storing one costs no storage-format change.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CheckpointDigest {
    bytes: [u8; 32],
}

/// Typed outcome of recomputing one fact's digest axis against a claimed
/// durable prefix ([`CheckpointFact::verify_digest`]). Mismatch and
/// corruption are reported, never smoothed into a pass: consumers must
/// treat them fail-closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckpointDigestVerification {
    /// The fact's digest axis is a well-formed structured digest equal to
    /// the one recomputed from the claimed prefix at the fact's own fence
    /// axes.
    Verified,
    /// The fact's digest axis is well-formed but differs from the
    /// recomputed one — a tampered claim, a wrong prefix, or fence axes
    /// moved under the digest. Fail closed.
    Mismatch {
        /// The digest the fact presented.
        presented: CheckpointDigest,
        /// The digest recomputed from the claimed prefix.
        recomputed: CheckpointDigest,
    },
    /// The fact's digest axis carries no structured-digest tag: author
    /// free text this module cannot compare. No verification claim may be
    /// derived from it (the honest `PARTIAL` / `UNCERTAIN` read of
    /// ADR-0019 R1.2.3), but it is not corruption either.
    FreeText,
    /// The digest axis tags itself as a structured digest but its body
    /// does not decode (wrong length, non-hex, or non-canonical casing).
    /// Fail closed; never silently re-read as free text.
    Corrupt,
}

/// Fail-closed errors of the federation mechanism face.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FederationError {
    /// The federation root or one of its sub-directories could not be
    /// created, read, or written.
    DirectoryUnavailable,
    /// A file this face owns is present but does not parse; fail closed
    /// instead of overwriting or skipping silently where correctness
    /// demands it.
    StateCorrupt,
    /// A publish would move the registration's `(boot, heartbeat)`
    /// backwards; the on-disk record wins.
    RegistrationNotMonotone {
        /// Heartbeat counter currently on disk.
        on_disk_heartbeat: u64,
        /// Heartbeat counter the caller presented.
        presented_heartbeat: u64,
    },
    /// A caller-supplied field is invalid (empty or multi-line text, or a
    /// source equal to the target).
    InvalidIntent(&'static str),
    /// A caller-supplied checkpoint field is invalid (empty, multi-line,
    /// or over-long digest text).
    InvalidCheckpoint(&'static str),
    /// The wall clock read before the Unix epoch, so no `heartbeat_ms` /
    /// `recorded_ms` can be assigned.
    WallClockUnavailable,
}

impl fmt::Display for FederationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DirectoryUnavailable => formatter.write_str("federation directory unavailable"),
            Self::StateCorrupt => formatter.write_str("federation state file is corrupt"),
            Self::RegistrationNotMonotone {
                on_disk_heartbeat,
                presented_heartbeat,
            } => write!(
                formatter,
                "registration heartbeat must advance: on disk {on_disk_heartbeat}, presented {presented_heartbeat}"
            ),
            Self::InvalidIntent(reason) => {
                write!(formatter, "invalid migration intent: {reason}")
            }
            Self::InvalidCheckpoint(reason) => {
                write!(formatter, "invalid reconciliation checkpoint: {reason}")
            }
            Self::WallClockUnavailable => {
                formatter.write_str("wall clock reads before the Unix epoch")
            }
        }
    }
}

impl std::error::Error for FederationError {}

impl CellDirectory {
    /// Opens (idempotently) the shared federation root, creating
    /// `<root>/cells/`, `<root>/migration-intents/`, and
    /// `<root>/checkpoints/` as needed.
    ///
    /// # Errors
    ///
    /// Fails with [`FederationError::DirectoryUnavailable`] when the root
    /// or a sub-directory cannot be created.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, FederationError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join(CELLS_DIR))
            .map_err(|_| FederationError::DirectoryUnavailable)?;
        fs::create_dir_all(root.join(INTENTS_DIR))
            .map_err(|_| FederationError::DirectoryUnavailable)?;
        fs::create_dir_all(root.join(CHECKPOINTS_DIR))
            .map_err(|_| FederationError::DirectoryUnavailable)?;
        Ok(Self { root })
    }

    /// The federation root this handle serves.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Publishes (first registration or heartbeat refresh) one Cell's
    /// registration: the file is replaced whole through write-temp +
    /// rename, and `heartbeat_ms` is stamped by this call. The caller
    /// supplies the monotone `heartbeat` counter (the file generation); a
    /// publish that would not advance `(boot, heartbeat)` beyond the
    /// on-disk record is refused.
    ///
    /// Only the owning Cell publishes its own file (filename = identity);
    /// concurrent foreign writers to one file are outside this face.
    ///
    /// # Errors
    ///
    /// Fails with [`FederationError::RegistrationNotMonotone`] on a stale
    /// counter, [`FederationError::StateCorrupt`] when the existing file
    /// does not parse (never overwritten), and
    /// [`FederationError::DirectoryUnavailable`] on I/O failure.
    pub fn publish_registration(
        &self,
        entry: &CellRegistrationEntry,
    ) -> Result<CellRegistrationEntry, FederationError> {
        let path = self.cell_file(entry.identity);
        if path
            .try_exists()
            .map_err(|_| FederationError::DirectoryUnavailable)?
        {
            let on_disk = Self::parse_registration_file(&path)?;
            let stale = (entry.node_boot_generation, entry.heartbeat)
                <= (on_disk.node_boot_generation, on_disk.heartbeat);
            if stale {
                return Err(FederationError::RegistrationNotMonotone {
                    on_disk_heartbeat: on_disk.heartbeat,
                    presented_heartbeat: entry.heartbeat,
                });
            }
        }
        let mut stamped = entry.clone();
        stamped.heartbeat_ms = wall_ms_since_epoch()?;
        atomic_write(&path, &staged_registration(&stamped))?;
        Ok(stamped)
    }

    /// Enumerates every registration under `<root>/cells/`, sorted by
    /// identity. One registration file per Cell; see
    /// [`CellDirectorySnapshot::corrupt`] for the unparseable tail.
    ///
    /// # Errors
    ///
    /// Fails with [`FederationError::DirectoryUnavailable`] when the
    /// directory cannot be listed or one file cannot be read.
    pub fn snapshot(&self) -> Result<CellDirectorySnapshot, FederationError> {
        let mut entries = Vec::new();
        let mut corrupt = 0;
        for path in self.files_with_ext(CELLS_DIR, CELL_FILE_EXT)? {
            match Self::parse_registration_file(&path) {
                Ok(entry) => entries.push(entry),
                Err(FederationError::StateCorrupt) => corrupt += 1,
                Err(error) => return Err(error),
            }
        }
        entries.sort_by_key(|entry| entry.identity);
        Ok(CellDirectorySnapshot { entries, corrupt })
    }

    /// Looks one Cell up by name (its stable identity): the discovery
    /// read face of "按名查 Cell". A miss is `None`.
    ///
    /// # Errors
    ///
    /// Fails with [`FederationError::DirectoryUnavailable`] on I/O
    /// failure and [`FederationError::StateCorrupt`] when the Cell's
    /// file exists but does not parse.
    pub fn find(
        &self,
        identity: CellIdentity,
    ) -> Result<Option<CellRegistrationEntry>, FederationError> {
        let path = self.cell_file(identity);
        if !path
            .try_exists()
            .map_err(|_| FederationError::DirectoryUnavailable)?
        {
            return Ok(None);
        }
        Ok(Some(Self::parse_registration_file(&path)?))
    }

    /// Enumerates the registrations whose last heartbeat is within
    /// `max_silence_ms` of now — the "枚举活 Cell" read face. Freshness is
    /// a discovery heuristic on the stamped `heartbeat_ms`, not a fencing
    /// oracle: no authority is granted or revoked here.
    ///
    /// Note (`ADR-0019` Trigger 7 #1, `ADR-0026` §2.3): this file-timestamp
    /// face is strictly for single-host shared-directory deployments
    /// (`ClusterProfile::SingleHostDesktop`). Cross-physical-host deployments
    /// MUST evaluate [`crate::CrossHostHeartbeatEnvelope`] against
    /// receiver-local monotonic windows via
    /// [`crate::ReceiverMonotonicLivenessTracker`] and order checkpoints via
    /// [`Self::record_monotonic_checkpoint`] /
    /// [`Self::read_monotonic_checkpoint_trail`].
    ///
    /// # Errors
    ///
    /// As [`Self::snapshot`], plus
    /// [`FederationError::WallClockUnavailable`] when now cannot be read.
    pub fn live_cells(
        &self,
        max_silence_ms: u64,
    ) -> Result<Vec<CellRegistrationEntry>, FederationError> {
        let now_ms = wall_ms_since_epoch()?;
        Ok(self
            .snapshot()?
            .into_entries()
            .into_iter()
            .filter(|entry| now_ms.saturating_sub(entry.heartbeat_ms) <= max_silence_ms)
            .collect())
    }

    /// Appends one migration intent to `<root>/migration-intents/` as an
    /// immutable file and returns the recorded form with the log's own
    /// ordering axes. Recording executes nothing.
    ///
    /// # Errors
    ///
    /// Fails with [`FederationError::InvalidIntent`] when the intent is
    /// malformed, [`FederationError::DirectoryUnavailable`] on I/O
    /// failure, and [`FederationError::WallClockUnavailable`] when the
    /// recording time cannot be stamped.
    pub fn record_migration_intent(
        &self,
        intent: &MigrationIntent,
    ) -> Result<MigrationIntentRecord, FederationError> {
        intent.validate()?;
        let recorded_ms = wall_ms_since_epoch()?;
        let sequence = INTENT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let record = MigrationIntentRecord {
            intent: intent.clone(),
            recorded_ms,
            os_process_id: std::process::id(),
            sequence,
        };
        let path = self.intent_file(&record);
        if path
            .try_exists()
            .map_err(|_| FederationError::DirectoryUnavailable)?
        {
            return Err(FederationError::DirectoryUnavailable);
        }
        atomic_write(&path, &staged_intent(&record))?;
        Ok(record)
    }

    /// Enumerates the migration-intent log in append order — sorted by
    /// (`recorded_ms`, `os_process_id`, `sequence`), the same axes that
    /// make filenames unique. See [`MigrationIntentLog::corrupt`] for the
    /// unparseable tail.
    ///
    /// # Errors
    ///
    /// Fails with [`FederationError::DirectoryUnavailable`] when the
    /// directory cannot be listed or one file cannot be read.
    pub fn migration_intents(&self) -> Result<MigrationIntentLog, FederationError> {
        let mut records = Vec::new();
        let mut corrupt = 0;
        for path in self.files_with_ext(INTENTS_DIR, INTENT_FILE_EXT)? {
            match Self::parse_intent_file(&path) {
                Ok(record) => records.push(record),
                Err(FederationError::StateCorrupt) => corrupt += 1,
                Err(error) => return Err(error),
            }
        }
        records.sort_by_key(|record| (record.recorded_ms, record.os_process_id, record.sequence));
        Ok(MigrationIntentLog { records, corrupt })
    }

    /// Appends one reconciliation/audit checkpoint of `cell` to
    /// `<root>/checkpoints/` as an immutable file and returns the recorded
    /// form with the log's own ordering axes. The owning Cell's identity
    /// becomes the filename prefix; recording verifies nothing and
    /// executes nothing (the fact is the author's claim, stored verbatim).
    ///
    /// # Errors
    ///
    /// Fails with [`FederationError::InvalidCheckpoint`] when the fact's
    /// digest is invalid, [`FederationError::DirectoryUnavailable`] on
    /// I/O failure, and [`FederationError::WallClockUnavailable`] when the
    /// recording time cannot be stamped.
    pub fn record_checkpoint(
        &self,
        cell: CellIdentity,
        fact: &CheckpointFact,
    ) -> Result<CheckpointRecord, FederationError> {
        fact.validate()?;
        let recorded_ms = wall_ms_since_epoch()?;
        let sequence = CHECKPOINT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let record = CheckpointRecord {
            cell,
            fact: fact.clone(),
            recorded_ms,
            os_process_id: std::process::id(),
            sequence,
        };
        let path = self.checkpoint_file(&record);
        if path
            .try_exists()
            .map_err(|_| FederationError::DirectoryUnavailable)?
        {
            return Err(FederationError::DirectoryUnavailable);
        }
        atomic_write(&path, &staged_checkpoint(&record))?;
        Ok(record)
    }

    /// Enumerates `cell`'s checkpoint audit trail in append order — only
    /// the files under that Cell's filename prefix are read, sorted by
    /// (`recorded_ms`, `os_process_id`, `sequence`), the same axes that
    /// make filenames unique. See [`CheckpointLog::corrupt`] for the
    /// unparseable/misfiled tail.
    ///
    /// # Errors
    ///
    /// Fails with [`FederationError::DirectoryUnavailable`] when the
    /// directory cannot be listed or one file cannot be read.
    pub fn checkpoints(&self, cell: CellIdentity) -> Result<CheckpointLog, FederationError> {
        let prefix = format!("{}-", hex_identity(cell.as_bytes()));
        let mut records = Vec::new();
        let mut corrupt = 0;
        for path in self.files_with_ext(CHECKPOINTS_DIR, CHECKPOINT_FILE_EXT)? {
            let matches_cell = path
                .file_name()
                .is_some_and(|name| name.as_encoded_bytes().starts_with(prefix.as_bytes()));
            if !matches_cell {
                continue;
            }
            match Self::parse_checkpoint_file(&path) {
                Ok(record) => {
                    if record.cell == cell {
                        records.push(record);
                    } else {
                        // A parseable file misfiled under this Cell's
                        // prefix but naming another Cell — a shape this
                        // face never writes: counted corrupt, never
                        // silently adopted into the trail.
                        corrupt += 1;
                    }
                }
                Err(FederationError::StateCorrupt) => corrupt += 1,
                Err(error) => return Err(error),
            }
        }
        records.sort_by_key(|record| (record.recorded_ms, record.os_process_id, record.sequence));
        Ok(CheckpointLog { records, corrupt })
    }

    fn cell_file(&self, identity: CellIdentity) -> PathBuf {
        let mut name = hex_identity(identity.as_bytes());
        name.push('.');
        name.push_str(CELL_FILE_EXT);
        self.root.join(CELLS_DIR).join(name)
    }

    fn intent_file(&self, record: &MigrationIntentRecord) -> PathBuf {
        let name = format!(
            "{:020}-{:010}-{:010}.{INTENT_FILE_EXT}",
            record.recorded_ms, record.os_process_id, record.sequence
        );
        self.root.join(INTENTS_DIR).join(name)
    }

    fn checkpoint_file(&self, record: &CheckpointRecord) -> PathBuf {
        let name = format!(
            "{}-{:020}-{:010}-{:010}.{CHECKPOINT_FILE_EXT}",
            hex_identity(record.cell.as_bytes()),
            record.recorded_ms,
            record.os_process_id,
            record.sequence
        );
        self.root.join(CHECKPOINTS_DIR).join(name)
    }

    fn files_with_ext(&self, dir: &str, ext: &str) -> Result<Vec<PathBuf>, FederationError> {
        let dir = self.root.join(dir);
        let mut paths = Vec::new();
        let read = fs::read_dir(&dir).map_err(|_| FederationError::DirectoryUnavailable)?;
        for entry in read {
            let entry = entry.map_err(|_| FederationError::DirectoryUnavailable)?;
            let path = entry.path();
            let matches = path.extension().is_some_and(|found| {
                found
                    .as_encoded_bytes()
                    .eq_ignore_ascii_case(ext.as_bytes())
            });
            if matches {
                paths.push(path);
            }
        }
        paths.sort();
        Ok(paths)
    }

    fn parse_registration_file(path: &Path) -> Result<CellRegistrationEntry, FederationError> {
        let raw = fs::read_to_string(path).map_err(|_| FederationError::DirectoryUnavailable)?;
        parse_registration(&raw).ok_or(FederationError::StateCorrupt)
    }

    fn parse_intent_file(path: &Path) -> Result<MigrationIntentRecord, FederationError> {
        let raw = fs::read_to_string(path).map_err(|_| FederationError::DirectoryUnavailable)?;
        parse_intent(&raw).ok_or(FederationError::StateCorrupt)
    }

    fn parse_checkpoint_file(path: &Path) -> Result<CheckpointRecord, FederationError> {
        let raw = fs::read_to_string(path).map_err(|_| FederationError::DirectoryUnavailable)?;
        parse_checkpoint(&raw).ok_or(FederationError::StateCorrupt)
    }
}

impl CellRegistrationEntry {
    /// Builds a registration from the Cell's current fence axes plus a
    /// one-line assembly description (the "pipeline 描述摘要" — this slice
    /// has no socket face). The heartbeat counter starts at 0; the
    /// publisher bumps it per refresh.
    ///
    /// # Errors
    ///
    /// Fails with [`FederationError::InvalidIntent`] when the description
    /// is empty, multi-line, or longer than `MAX_TEXT_BYTES`.
    pub fn new(
        identity: CellIdentity,
        node_boot_generation: Generation,
        epoch: CellEpoch,
        fencing_token: CellFencingToken,
        os_process_id: u32,
        description: &str,
    ) -> Result<Self, FederationError> {
        validate_text("description", description)?;
        Ok(Self {
            identity,
            node_boot_generation,
            epoch,
            fencing_token,
            os_process_id,
            description: description.to_string(),
            heartbeat: 0,
            heartbeat_ms: 0,
        })
    }

    /// Sets the heartbeat counter (the registration file's generation);
    /// every publish must advance it beyond the on-disk record.
    #[must_use]
    pub fn with_heartbeat(mut self, heartbeat: u64) -> Self {
        self.heartbeat = heartbeat;
        self
    }

    /// Stable identity of the registering Cell.
    #[must_use]
    pub const fn identity(&self) -> CellIdentity {
        self.identity
    }

    /// Boot generation of the publishing process claim.
    #[must_use]
    pub const fn node_boot_generation(&self) -> Generation {
        self.node_boot_generation
    }

    /// Cell epoch last published.
    #[must_use]
    pub const fn epoch(&self) -> CellEpoch {
        self.epoch
    }

    /// Fencing token last published.
    #[must_use]
    pub const fn fencing_token(&self) -> CellFencingToken {
        self.fencing_token
    }

    /// OS process that published the record. Observational only; not part
    /// of the stable identity.
    #[must_use]
    pub const fn os_process_id(&self) -> u32 {
        self.os_process_id
    }

    /// One-line assembly description digest.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Heartbeat counter — the registration file's monotone generation.
    #[must_use]
    pub const fn heartbeat(&self) -> u64 {
        self.heartbeat
    }

    /// Wall millis (Unix epoch) the publisher stamped this record with.
    #[must_use]
    pub const fn heartbeat_ms(&self) -> u64 {
        self.heartbeat_ms
    }
}

impl CellDirectorySnapshot {
    /// Every parseable registration, sorted by identity.
    #[must_use]
    pub fn into_entries(self) -> Vec<CellRegistrationEntry> {
        self.entries
    }

    /// How many registration files under `<root>/cells/` failed to parse.
    #[must_use]
    pub const fn corrupt(&self) -> usize {
        self.corrupt
    }
}

impl MigrationObject {
    /// Wraps 16 opaque bytes as the object reference of an intent.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Deterministically projects a [`DistTaskRecordBinding`] into its
    /// 16-byte [`MigrationObject`] reference (`CS1-D`).
    #[must_use]
    pub fn from_dist_task_binding(binding: &DistTaskRecordBinding) -> Self {
        binding.migration_object()
    }

    /// The opaque bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl DistTaskMigrationKind {
    /// One-byte wire discriminant used inside [`DistTaskRecordBinding`]
    /// canonical entry bytes and digest preimages.
    #[must_use]
    pub const fn wire_code(self) -> u8 {
        match self {
            Self::AuthorityAssignment => 1,
            Self::TakeoverReceipt => 2,
            Self::ParticipantRegistry => 3,
        }
    }

    /// Strict inverse of [`Self::wire_code`].
    #[must_use]
    pub const fn from_wire_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::AuthorityAssignment),
            2 => Some(Self::TakeoverReceipt),
            3 => Some(Self::ParticipantRegistry),
            _ => None,
        }
    }
}

impl DistTaskRecordBinding {
    /// Returns `true` when every generation/term axis is non-zero (`>= 1`).
    #[must_use]
    pub const fn is_well_formed(&self) -> bool {
        self.task_generation >= 1
            && self.authority_term >= 1
            && self.participant_registry_generation >= 1
    }

    /// Canonical deterministic [`CheckpointPrefix`] entry byte grammar:
    /// `DTBINDV1 (8B) || kind (1B) || task_id (16B) || be64(task_generation) ||
    /// record_id (16B) || be64(authority_term) || be64(control_epoch) ||
    /// be64(participant_registry_generation) || participant_registry_root (32B) ||
    /// (0u8 | 1u8 || exact_fence_set_root (32B))`.
    #[must_use]
    pub fn to_checkpoint_entry(&self) -> Vec<u8> {
        let capacity = if self.exact_fence_set_root.is_some() {
            138
        } else {
            106
        };
        let mut out = Vec::with_capacity(capacity);
        out.extend_from_slice(DIST_TASK_ENTRY_MAGIC);
        out.push(self.kind.wire_code());
        out.extend_from_slice(&self.task_id);
        out.extend_from_slice(&self.task_generation.to_be_bytes());
        out.extend_from_slice(&self.record_id);
        out.extend_from_slice(&self.authority_term.to_be_bytes());
        out.extend_from_slice(&self.control_epoch.to_be_bytes());
        out.extend_from_slice(&self.participant_registry_generation.to_be_bytes());
        out.extend_from_slice(&self.participant_registry_root);
        match self.exact_fence_set_root {
            None => out.push(0),
            Some(root) => {
                out.push(1);
                out.extend_from_slice(&root);
            }
        }
        out
    }

    /// Strict inverse of [`Self::to_checkpoint_entry`]: decodes only a
    /// canonical, well-formed `DTBINDV1` entry (`None` on wrong magic,
    /// unknown kind, wrong byte length, non-canonical option flag, or a
    /// zero generation/term axis).
    #[must_use]
    pub fn from_checkpoint_entry(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 106 || &bytes[..8] != DIST_TASK_ENTRY_MAGIC {
            return None;
        }
        let kind = DistTaskMigrationKind::from_wire_code(bytes[8])?;
        let task_id: [u8; 16] = bytes[9..25].try_into().ok()?;
        let task_generation = u64::from_be_bytes(bytes[25..33].try_into().ok()?);
        let record_id: [u8; 16] = bytes[33..49].try_into().ok()?;
        let authority_term = u64::from_be_bytes(bytes[49..57].try_into().ok()?);
        let control_epoch = u64::from_be_bytes(bytes[57..65].try_into().ok()?);
        let participant_registry_generation = u64::from_be_bytes(bytes[65..73].try_into().ok()?);
        let participant_registry_root: [u8; 32] = bytes[73..105].try_into().ok()?;
        let exact_fence_set_root = match bytes[105] {
            0 => {
                if bytes.len() != 106 {
                    return None;
                }
                None
            }
            1 => {
                if bytes.len() != 138 {
                    return None;
                }
                Some(bytes[106..138].try_into().ok()?)
            }
            _ => return None,
        };
        let binding = Self {
            kind,
            task_id,
            task_generation,
            record_id,
            authority_term,
            control_epoch,
            participant_registry_generation,
            participant_registry_root,
            exact_fence_set_root,
        };
        binding.is_well_formed().then_some(binding)
    }

    /// Domain-separated `SHA-256` commitment over [`DIST_TASK_BINDING_DOMAIN`]
    /// (`"llmos/dist-task-migration-binding/v1"`) and [`Self::to_checkpoint_entry`].
    #[must_use]
    pub fn binding_digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(DIST_TASK_BINDING_DOMAIN);
        hasher.update(self.to_checkpoint_entry());
        hasher.finalize().into()
    }

    /// Deterministic 16-byte projection into [`MigrationObject`] (the first
    /// 16 bytes of [`Self::binding_digest`]).
    #[must_use]
    pub fn migration_object(&self) -> MigrationObject {
        let digest = self.binding_digest();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        MigrationObject::from_bytes(bytes)
    }

    /// Canonical single-line reason text (`v1:dist-task-binding:<64 lowercase hex>`,
    /// 85 bytes) carrying the full 32-byte [`Self::binding_digest`] inside a
    /// [`MigrationIntent`] without altering the on-disk intent file format.
    #[must_use]
    pub fn to_intent_reason(&self) -> String {
        let digest = self.binding_digest();
        let mut out = String::with_capacity(STRUCTURED_INTENT_REASON_TAG.len() + 64);
        out.push_str(STRUCTURED_INTENT_REASON_TAG);
        for byte in digest {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }
}

impl MigrationIntent {
    /// Builds an intent: `source` Cell hands `object` (at its fenced
    /// `generation`) to `target` Cell, for the stated one-line reason
    /// digest. Execution is out of scope (`C-MIGRATE`).
    ///
    /// # Errors
    ///
    /// Fails with [`FederationError::InvalidIntent`] when source equals
    /// target or the reason is empty, multi-line, or over-long.
    pub fn new(
        source: CellIdentity,
        target: CellIdentity,
        object: MigrationObject,
        generation: Generation,
        reason: &str,
    ) -> Result<Self, FederationError> {
        if source == target {
            return Err(FederationError::InvalidIntent(
                "source and target cells are the same",
            ));
        }
        validate_text("reason", reason)?;
        Ok(Self {
            source,
            target,
            object,
            generation,
            reason: reason.to_string(),
        })
    }

    /// The Cell that currently holds the object.
    #[must_use]
    pub const fn source(&self) -> CellIdentity {
        self.source
    }

    /// The Cell the object is intended to move to.
    #[must_use]
    pub const fn target(&self) -> CellIdentity {
        self.target
    }

    /// The opaque object reference.
    #[must_use]
    pub const fn object(&self) -> MigrationObject {
        self.object
    }

    /// The object's fenced generation the intent names.
    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    /// The one-line reason digest.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Builds a [`MigrationIntent`] cryptographically bound to a
    /// [`DistTaskRecordBinding`] (`CS1-D`): `object` is
    /// [`DistTaskRecordBinding::migration_object`], `generation` is the
    /// bound `authority_term`, and `reason` is the canonical
    /// `v1:dist-task-binding:<64 lowercase hex>` digest tag.
    ///
    /// # Errors
    ///
    /// Fails with [`FederationError::InvalidIntent`] when `source == target`
    /// or any generation/term axis in `binding` is zero.
    pub fn from_dist_task_binding(
        source: CellIdentity,
        target: CellIdentity,
        binding: &DistTaskRecordBinding,
    ) -> Result<Self, FederationError> {
        if !binding.is_well_formed() {
            return Err(FederationError::InvalidIntent(
                "dist-task binding generation and term axes must be non-zero",
            ));
        }
        let generation = Generation::new(NonZeroU64::new(binding.authority_term).ok_or(
            FederationError::InvalidIntent("dist-task binding authority_term must be non-zero"),
        )?);
        let reason = binding.to_intent_reason();
        Self::new(
            source,
            target,
            binding.migration_object(),
            generation,
            &reason,
        )
    }

    /// Verifies that this [`MigrationIntent`] cryptographically binds
    /// `expected` (`CS1-D`), fail-closed.
    #[must_use]
    pub fn verify_dist_task_binding(
        &self,
        expected: &DistTaskRecordBinding,
    ) -> DistTaskBindingVerification {
        if !expected.is_well_formed() {
            return DistTaskBindingVerification::Corrupt;
        }
        let expected_digest = expected.binding_digest();
        let generation_matches = self.generation.get() == expected.authority_term
            || self.generation.get() == expected.task_generation;
        let object_matches = self.object == expected.migration_object();
        let Some(body) = self.reason.strip_prefix(STRUCTURED_INTENT_REASON_TAG) else {
            if !object_matches || !generation_matches {
                return DistTaskBindingVerification::Mismatch {
                    expected_digest,
                    presented_digest: None,
                };
            }
            return DistTaskBindingVerification::Unbound;
        };
        let Some(presented_digest) = decode_lowercase_hex32(body) else {
            return DistTaskBindingVerification::Corrupt;
        };
        if presented_digest != expected_digest || !object_matches || !generation_matches {
            return DistTaskBindingVerification::Mismatch {
                expected_digest,
                presented_digest: Some(presented_digest),
            };
        }
        DistTaskBindingVerification::Verified
    }

    fn validate(&self) -> Result<(), FederationError> {
        if self.source == self.target {
            return Err(FederationError::InvalidIntent(
                "source and target cells are the same",
            ));
        }
        validate_text("reason", &self.reason)
    }
}

impl MigrationIntentRecord {
    /// The author's intent.
    #[must_use]
    pub const fn intent(&self) -> &MigrationIntent {
        &self.intent
    }

    /// Wall millis (Unix epoch) the log accepted the intent.
    #[must_use]
    pub const fn recorded_ms(&self) -> u64 {
        self.recorded_ms
    }

    /// OS process that recorded the intent (a log axis, not authority).
    #[must_use]
    pub const fn os_process_id(&self) -> u32 {
        self.os_process_id
    }

    /// Process-unique sequence of this record (a log axis).
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
}

impl MigrationIntentLog {
    /// Every parseable record, in append order.
    #[must_use]
    pub fn into_records(self) -> Vec<MigrationIntentRecord> {
        self.records
    }

    /// How many intent files failed to parse.
    #[must_use]
    pub const fn corrupt(&self) -> usize {
        self.corrupt
    }
}

impl CheckpointFact {
    /// Builds a checkpoint fact from the fence snapshot the fact is taken
    /// at (boot generation, epoch, fencing token) plus a one-line digest
    /// of what the author claims reconciled (for example a high-water
    /// summary of its own authorities).
    ///
    /// # Errors
    ///
    /// Fails with [`FederationError::InvalidCheckpoint`] when the digest
    /// is empty, multi-line, or longer than `MAX_TEXT_BYTES`.
    pub fn new(
        node_boot_generation: Generation,
        epoch: CellEpoch,
        fencing_token: CellFencingToken,
        digest: &str,
    ) -> Result<Self, FederationError> {
        validate_checkpoint_digest(digest)?;
        Ok(Self {
            node_boot_generation,
            epoch,
            fencing_token,
            digest: digest.to_string(),
        })
    }

    /// Boot generation of the process claim the fact's fence belongs to.
    #[must_use]
    pub const fn node_boot_generation(&self) -> Generation {
        self.node_boot_generation
    }

    /// Cell epoch the fact was taken at.
    #[must_use]
    pub const fn epoch(&self) -> CellEpoch {
        self.epoch
    }

    /// Fencing token the fact was taken at.
    #[must_use]
    pub const fn fencing_token(&self) -> CellFencingToken {
        self.fencing_token
    }

    /// The one-line digest of what the author claims reconciled.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Recomputes this fact's digest over `prefix` at the fact's *own*
    /// fence axes and compares, fail-closed. The digest pins the pair
    /// (fence snapshot, prefix bytes); comparing the axes themselves
    /// against a fence the caller knows is the executor's job, not this
    /// check.
    ///
    /// The outcome is typed ([`CheckpointDigestVerification`]): a
    /// well-formed equal digest is [`CheckpointDigestVerification::Verified`];
    /// a well-formed unequal one is
    /// [`CheckpointDigestVerification::Mismatch`] (tampered claim, wrong
    /// prefix, or moved axes — all fail closed); a digest axis without the
    /// structured tag is [`CheckpointDigestVerification::FreeText`]
    /// (comparable to nothing, and said so); a tagged body that does not
    /// decode is [`CheckpointDigestVerification::Corrupt`]. This function
    /// is infallible precisely because every failure mode is a value, not
    /// an exception to be swallowed.
    #[must_use]
    pub fn verify_digest(&self, prefix: &CheckpointPrefix) -> CheckpointDigestVerification {
        let text = self.digest.as_str();
        if !text.starts_with(STRUCTURED_DIGEST_TAG) {
            return CheckpointDigestVerification::FreeText;
        }
        let Some(presented) = CheckpointDigest::from_text(text) else {
            return CheckpointDigestVerification::Corrupt;
        };
        let recomputed = CheckpointDigest::from_prefix(
            self.node_boot_generation,
            self.epoch,
            self.fencing_token,
            prefix,
        );
        if presented == recomputed {
            CheckpointDigestVerification::Verified
        } else {
            CheckpointDigestVerification::Mismatch {
                presented,
                recomputed,
            }
        }
    }

    /// Verifies both this [`CheckpointFact`]'s digest against `prefix` and
    /// that `prefix` contains the exact canonical [`DistTaskRecordBinding`]
    /// entry for `expected` (`CS1-D`), fail-closed.
    #[must_use]
    pub fn verify_dist_task_binding(
        &self,
        prefix: &CheckpointPrefix,
        expected: &DistTaskRecordBinding,
    ) -> DistTaskBindingVerification {
        if !expected.is_well_formed() {
            return DistTaskBindingVerification::Corrupt;
        }
        match self.verify_digest(prefix) {
            CheckpointDigestVerification::Verified => {}
            CheckpointDigestVerification::Mismatch {
                presented,
                recomputed,
            } => {
                return DistTaskBindingVerification::CheckpointDigestMismatch {
                    presented,
                    recomputed,
                };
            }
            CheckpointDigestVerification::FreeText => {
                return DistTaskBindingVerification::Unbound;
            }
            CheckpointDigestVerification::Corrupt => {
                return DistTaskBindingVerification::Corrupt;
            }
        }
        let decoded = match prefix.decode_dist_task_bindings() {
            Ok(bindings) => bindings,
            Err(err) => return err,
        };
        let expected_digest = expected.binding_digest();
        if decoded.is_empty() {
            return DistTaskBindingVerification::Unbound;
        }
        let mut found_match = false;
        let mut first_mismatch_digest = None;
        for candidate in &decoded {
            if candidate == expected {
                found_match = true;
            } else if (candidate.kind == expected.kind
                && candidate.task_id == expected.task_id
                && candidate.record_id == expected.record_id)
                || first_mismatch_digest.is_none()
            {
                first_mismatch_digest = Some(candidate.binding_digest());
                if candidate.kind == expected.kind
                    && candidate.task_id == expected.task_id
                    && candidate.record_id == expected.record_id
                {
                    return DistTaskBindingVerification::Mismatch {
                        expected_digest,
                        presented_digest: Some(candidate.binding_digest()),
                    };
                }
            }
        }
        if found_match {
            DistTaskBindingVerification::Verified
        } else {
            DistTaskBindingVerification::Mismatch {
                expected_digest,
                presented_digest: first_mismatch_digest,
            }
        }
    }

    fn validate(&self) -> Result<(), FederationError> {
        validate_checkpoint_digest(&self.digest)
    }
}

impl CheckpointRecord {
    /// The owning Cell whose audit trail this record belongs to.
    #[must_use]
    pub const fn cell(&self) -> CellIdentity {
        self.cell
    }

    /// The author's fact.
    #[must_use]
    pub const fn fact(&self) -> &CheckpointFact {
        &self.fact
    }

    /// Wall millis (Unix epoch) the log accepted the checkpoint.
    #[must_use]
    pub const fn recorded_ms(&self) -> u64 {
        self.recorded_ms
    }

    /// OS process that recorded the checkpoint (a log axis, not
    /// authority).
    #[must_use]
    pub const fn os_process_id(&self) -> u32 {
        self.os_process_id
    }

    /// Process-unique sequence of this record (a log axis).
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
}

impl CheckpointLog {
    /// Every parseable record, in append order.
    #[must_use]
    pub fn into_records(self) -> Vec<CheckpointRecord> {
        self.records
    }

    /// How many checkpoint files under the Cell's prefix failed to parse
    /// or parsed into a record naming a different Cell.
    #[must_use]
    pub const fn corrupt(&self) -> usize {
        self.corrupt
    }
}

impl CheckpointPrefix {
    /// The empty prefix — the "nothing committed yet" claim.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Builds a prefix from its ordered opaque entries, first entry the
    /// oldest. Entries may be empty themselves; the grammar keeps them
    /// distinct from "no entry".
    #[must_use]
    pub fn from_entries(entries: impl IntoIterator<Item: AsRef<[u8]>>) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(|entry| entry.as_ref().to_vec())
                .collect(),
        }
    }

    /// Builds a [`CheckpointPrefix`] from an ordered sequence of
    /// [`DistTaskRecordBinding`] descriptors (`CS1-D`), serializing each
    /// binding through [`DistTaskRecordBinding::to_checkpoint_entry`].
    #[must_use]
    pub fn from_dist_task_bindings<'a>(
        bindings: impl IntoIterator<Item = &'a DistTaskRecordBinding>,
    ) -> Self {
        Self {
            entries: bindings
                .into_iter()
                .map(DistTaskRecordBinding::to_checkpoint_entry)
                .collect(),
        }
    }

    /// Decodes all `DTBINDV1`-tagged [`DistTaskRecordBinding`] entries in
    /// this prefix (`CS1-D`).
    ///
    /// # Errors
    ///
    /// Fails closed with [`DistTaskBindingVerification::Corrupt`] if any
    /// entry starting with the `DTBINDV1` magic header is malformed or has
    /// a zero generation/term axis.
    pub fn decode_dist_task_bindings(
        &self,
    ) -> Result<Vec<DistTaskRecordBinding>, DistTaskBindingVerification> {
        let mut out = Vec::new();
        for entry in &self.entries {
            if entry.starts_with(DIST_TASK_ENTRY_MAGIC) {
                let binding = DistTaskRecordBinding::from_checkpoint_entry(entry)
                    .ok_or(DistTaskBindingVerification::Corrupt)?;
                out.push(binding);
            }
        }
        Ok(out)
    }

    /// The ordered opaque entries, in prefix order.
    #[must_use]
    pub fn entries(&self) -> &[Vec<u8>] {
        &self.entries
    }

    /// Canonical byte grammar every digest is taken over:
    /// `be64(count) || (be64(len) || bytes)*` — count first, then each
    /// entry length-prefixed big-endian, so two distinct entry sequences
    /// never share a serialization (for example `[b"ab", b"c"]` and
    /// `[b"abc"]` differ).
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + 8 * self.entries.len());
        out.extend_from_slice(
            &u64::try_from(self.entries.len())
                .expect("count fits u64")
                .to_be_bytes(),
        );
        for entry in &self.entries {
            out.extend_from_slice(
                &u64::try_from(entry.len())
                    .expect("entry fits u64")
                    .to_be_bytes(),
            );
            out.extend_from_slice(entry);
        }
        out
    }
}

impl CheckpointDigest {
    /// Derives the structured digest of one checkpoint claim:
    /// `SHA-256("llmos/reconciliation-checkpoint/v1" || be64(boot) ||
    /// be64(epoch) || be64(token) || canonical(prefix))`. Deterministic in
    /// every input; any axis or prefix-byte change changes the digest.
    #[must_use]
    pub fn from_prefix(
        node_boot_generation: Generation,
        epoch: CellEpoch,
        fencing_token: CellFencingToken,
        prefix: &CheckpointPrefix,
    ) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(CHECKPOINT_DIGEST_DOMAIN);
        hasher.update(node_boot_generation.get().to_be_bytes());
        hasher.update(epoch.get().to_be_bytes());
        hasher.update(fencing_token.get().to_be_bytes());
        hasher.update(prefix.canonical_bytes());
        Self {
            bytes: hasher.finalize().into(),
        }
    }

    /// The 32 raw digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.bytes
    }

    /// The canonical single-line text form `v1:sha256:<64 lowercase hex>`
    /// (74 bytes, inside the fact's `MAX_TEXT_BYTES` single-line bound —
    /// the storage format does not change to carry one).
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = String::with_capacity(STRUCTURED_DIGEST_TAG.len() + 64);
        out.push_str(STRUCTURED_DIGEST_TAG);
        for byte in self.bytes {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    /// Strict inverse of [`Self::to_text`]: only the exact canonical form
    /// decodes (`None` otherwise) — same digest bytes have exactly one
    /// accepted spelling, so byte-exact replay discipline never has to
    /// forgive an encoding drift.
    fn from_text(text: &str) -> Option<Self> {
        let body = text.strip_prefix(STRUCTURED_DIGEST_TAG)?;
        decode_lowercase_hex32(body).map(|bytes| Self { bytes })
    }
}

/// Verifies both a [`MigrationIntent`] and a [`CheckpointFact`] +
/// [`CheckpointPrefix`] against an expected [`DistTaskRecordBinding`]
/// (`CS1-D`), fail-closed.
#[must_use]
pub fn verify_dist_task_binding(
    intent: &MigrationIntent,
    fact: &CheckpointFact,
    prefix: &CheckpointPrefix,
    expected: &DistTaskRecordBinding,
) -> DistTaskBindingVerification {
    match intent.verify_dist_task_binding(expected) {
        DistTaskBindingVerification::Verified => fact.verify_dist_task_binding(prefix, expected),
        other => other,
    }
}

fn decode_lowercase_hex32(body: &str) -> Option<[u8; 32]> {
    let raw = body.as_bytes();
    if raw.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, chunk) in raw.chunks(2).enumerate() {
        let high = lowercase_hex_nibble(chunk[0])?;
        let low = lowercase_hex_nibble(chunk[1])?;
        bytes[index] = (high << 4) | low;
    }
    Some(bytes)
}

fn staged_registration(entry: &CellRegistrationEntry) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "cell={}", hex_identity(entry.identity.as_bytes()));
    let _ = writeln!(out, "boot={}", entry.node_boot_generation.get());
    let _ = writeln!(out, "epoch={}", entry.epoch.get());
    let _ = writeln!(out, "token={}", entry.fencing_token.get());
    let _ = writeln!(out, "pid={}", entry.os_process_id);
    let _ = writeln!(out, "heartbeat={}", entry.heartbeat);
    let _ = writeln!(out, "heartbeat_ms={}", entry.heartbeat_ms);
    let _ = write!(out, "summary={}", entry.description);
    out
}

fn parse_registration(raw: &str) -> Option<CellRegistrationEntry> {
    let fields = parse_keyed(raw)?;
    let identity = CellIdentity::from_domain(nlos_types::SchedulerDomainId::from_bytes(hex16(
        fields.get("cell")?,
    )?));
    let node_boot_generation = Generation::new(NonZeroU64::new(u64_field(&fields, "boot")?)?);
    let epoch = CellEpoch::from_u64(u64_field(&fields, "epoch")?)?;
    let fencing_token = CellFencingToken::from_u64(u64_field(&fields, "token")?)?;
    let os_process_id = u32::try_from(u64_field(&fields, "pid")?).ok()?;
    let heartbeat = u64_field(&fields, "heartbeat")?;
    let heartbeat_ms = u64_field(&fields, "heartbeat_ms")?;
    let description = fields.get("summary")?;
    if description.is_empty() || description.contains(['\n', '\r']) {
        return None;
    }
    Some(CellRegistrationEntry {
        identity,
        node_boot_generation,
        epoch,
        fencing_token,
        os_process_id,
        description: description.to_string(),
        heartbeat,
        heartbeat_ms,
    })
}

fn staged_intent(record: &MigrationIntentRecord) -> String {
    let intent = &record.intent;
    let mut out = String::new();
    let _ = writeln!(out, "source={}", hex_identity(intent.source.as_bytes()));
    let _ = writeln!(out, "target={}", hex_identity(intent.target.as_bytes()));
    let _ = writeln!(out, "object={}", hex_identity(intent.object.as_bytes()));
    let _ = writeln!(out, "generation={}", intent.generation.get());
    let _ = writeln!(out, "recorded_ms={}", record.recorded_ms);
    let _ = writeln!(out, "pid={}", record.os_process_id);
    let _ = writeln!(out, "sequence={}", record.sequence);
    let _ = write!(out, "reason={}", intent.reason);
    out
}

fn parse_intent(raw: &str) -> Option<MigrationIntentRecord> {
    let fields = parse_keyed(raw)?;
    let source = CellIdentity::from_domain(nlos_types::SchedulerDomainId::from_bytes(hex16(
        fields.get("source")?,
    )?));
    let target = CellIdentity::from_domain(nlos_types::SchedulerDomainId::from_bytes(hex16(
        fields.get("target")?,
    )?));
    let object = MigrationObject::from_bytes(hex16(fields.get("object")?)?);
    let generation = Generation::new(NonZeroU64::new(u64_field(&fields, "generation")?)?);
    let recorded_ms = u64_field(&fields, "recorded_ms")?;
    let os_process_id = u32::try_from(u64_field(&fields, "pid")?).ok()?;
    let sequence = u64_field(&fields, "sequence")?;
    let reason = fields.get("reason")?;
    if reason.is_empty() || reason.contains(['\n', '\r']) {
        return None;
    }
    if source == target {
        return None;
    }
    Some(MigrationIntentRecord {
        intent: MigrationIntent {
            source,
            target,
            object,
            generation,
            reason: reason.to_string(),
        },
        recorded_ms,
        os_process_id,
        sequence,
    })
}

fn staged_checkpoint(record: &CheckpointRecord) -> String {
    let fact = &record.fact;
    let mut out = String::new();
    let _ = writeln!(out, "cell={}", hex_identity(record.cell.as_bytes()));
    let _ = writeln!(out, "boot={}", fact.node_boot_generation.get());
    let _ = writeln!(out, "epoch={}", fact.epoch.get());
    let _ = writeln!(out, "token={}", fact.fencing_token.get());
    let _ = writeln!(out, "recorded_ms={}", record.recorded_ms);
    let _ = writeln!(out, "pid={}", record.os_process_id);
    let _ = writeln!(out, "sequence={}", record.sequence);
    let _ = write!(out, "digest={}", fact.digest);
    out
}

fn parse_checkpoint(raw: &str) -> Option<CheckpointRecord> {
    let fields = parse_keyed(raw)?;
    let cell = CellIdentity::from_domain(nlos_types::SchedulerDomainId::from_bytes(hex16(
        fields.get("cell")?,
    )?));
    let node_boot_generation = Generation::new(NonZeroU64::new(u64_field(&fields, "boot")?)?);
    let epoch = CellEpoch::from_u64(u64_field(&fields, "epoch")?)?;
    let fencing_token = CellFencingToken::from_u64(u64_field(&fields, "token")?)?;
    let recorded_ms = u64_field(&fields, "recorded_ms")?;
    let os_process_id = u32::try_from(u64_field(&fields, "pid")?).ok()?;
    let sequence = u64_field(&fields, "sequence")?;
    let digest = fields.get("digest")?;
    if digest.is_empty() || digest.contains(['\n', '\r']) {
        return None;
    }
    Some(CheckpointRecord {
        cell,
        fact: CheckpointFact {
            node_boot_generation,
            epoch,
            fencing_token,
            digest: digest.to_string(),
        },
        recorded_ms,
        os_process_id,
        sequence,
    })
}

/// Splits a staged file into its `key=value` fields. Strict: a duplicated
/// or unknown key fails the parse (fail closed on shapes this face never
/// writes).
fn parse_keyed(raw: &str) -> Option<std::collections::BTreeMap<&str, &str>> {
    let mut fields = std::collections::BTreeMap::new();
    for line in raw.lines() {
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once('=')?;
        if key.is_empty() || value.is_empty() {
            return None;
        }
        if fields.insert(key, value).is_some() {
            return None;
        }
    }
    Some(fields)
}

fn u64_field(fields: &std::collections::BTreeMap<&str, &str>, key: &str) -> Option<u64> {
    fields.get(key).copied()?.parse().ok()
}

fn hex16(text: &str) -> Option<[u8; 16]> {
    if text.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
        let high = hex_nibble(chunk[0])?;
        let low = hex_nibble(chunk[1])?;
        bytes[index] = (high << 4) | low;
    }
    Some(bytes)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Strictly lowercase hex nibble: the structured digest's canonical text
/// form is lowercase-only, so uppercase spellings decode to `None` (corrupt)
/// instead of silently equaling the canonical one.
const fn lowercase_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn hex_identity(bytes: &[u8; 16]) -> String {
    let mut out = String::with_capacity(32);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn validate_text(field: &'static str, text: &str) -> Result<(), FederationError> {
    if text.is_empty() || text.len() > MAX_TEXT_BYTES || text.contains(['\n', '\r']) {
        return Err(FederationError::InvalidIntent(field));
    }
    Ok(())
}

fn validate_checkpoint_digest(digest: &str) -> Result<(), FederationError> {
    if digest.is_empty() || digest.len() > MAX_TEXT_BYTES || digest.contains(['\n', '\r']) {
        return Err(FederationError::InvalidCheckpoint("digest"));
    }
    Ok(())
}

fn wall_ms_since_epoch() -> Result<u64, FederationError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        // Millis-since-epoch saturates long past any wall clock this face
        // will read; the conversion is exact for every real clock value.
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .map_err(|_| FederationError::WallClockUnavailable)
}

fn atomic_write(path: &Path, contents: &str) -> Result<(), FederationError> {
    let parent = path.parent().ok_or(FederationError::DirectoryUnavailable)?;
    let file_name = path
        .file_name()
        .ok_or(FederationError::DirectoryUnavailable)?
        .to_string_lossy();
    let tmp = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));
    fs::write(&tmp, contents).map_err(|_| FederationError::DirectoryUnavailable)?;
    fs::rename(&tmp, path).map_err(|_| {
        let _ = fs::remove_file(&tmp);
        FederationError::DirectoryUnavailable
    })
}
