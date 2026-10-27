//! Durable `SQLite WAL + synchronous=FULL` persistence engine for
//! `nlos-consensus` (`ADR-0026` §3.3 `C-CONSENSUS-CP`, `[DUR-ACK-002]`,
//! `[DUR-SNAP-001]`, `[DUR-FAILCLOSED-001]`).
//!
//! # Enforced Durability & Fail-Closed Invariants
//!
//! 1. **Mandatory `WAL + FULL` Durability**:
//!    Every connection configures and verifies `journal_mode = WAL`,
//!    `synchronous = FULL`, and `foreign_keys = ON`. Any inability to honor
//!    full durability fails closed with
//!    [`ConsensusStoreError::DurabilityUnavailable`].
//! 2. **Atomic Hard-State & Single-Vote Enforcement**:
//!    `(current_term, voted_for, commit_index, hard_state_sha256)` lives in a
//!    `STRICT` singleton table guarded by `BEFORE UPDATE` triggers preventing
//!    term regression, commit regression, and double-voting within a term.
//! 3. **Per-Entry `SHA-256` & Contiguity Verification (`[DUR-FAILCLOSED-001]`)**:
//!    Every log row stores `(log_index, term, command_kind, idempotency_key,
//!    payload_bytes, entry_sha256)` in a `STRICT` table with
//!    `CHECK (command_kind IN (1, 2, 3, 4))`. Recovery and readbacks verify
//!    `PRAGMA quick_check`, `hard_state_sha256`, every `entry_sha256`,
//!    `snapshot_sha256`, and contiguous `log_index` coverage up to
//!    `commit_index` — never silently truncating corrupted state.
//! 4. **Crash-Safe Snapshot & Compaction (`[DUR-SNAP-001]`)**:
//!    Snapshots write to `<dir>/snapshot-<idx>-<term>.tmp`, `sync_all()`,
//!    atomically rename to `.bin`, and commit metadata in `SQLite WAL+FULL`
//!    before retiring superseded snapshot files or compacting covered log rows.

use std::error::Error;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use nlos_cell::CellIdentity;
use nlos_types::{IdempotencyKey, SchedulerDomainId};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::core::{
    ConsensusEffect, ConsensusTerm, LogIndex, RaftLogEntry, RehydratedCoreState, SnapshotMetadata,
    compute_log_entry_sha256, compute_snapshot_transfer_sha256,
};
use crate::state_machine::{
    AppliedProposalOutcome, CommandFamilyCode, ControlPlaneProposal, ControlPlaneStateMachine,
    ScopeViolationError, SnapshotCodecError,
};

const HARD_STATE_HASH_DOMAIN: &[u8] = b"llmos/consensus/hard-state/v1";
const DB_FILE_NAME: &str = "consensus.sqlite3";

/// Authentic commit receipt returned ONLY after the entry is durably persisted
/// (`SQLite WAL + synchronous=FULL`) on a majority of independent voter
/// domains (`[DUR-ACK-002]`, or local `N = 1` passthrough transaction).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConsensusCommitReceipt {
    /// Majority-committed 1-based Raft log index (`[DUR-ACK-002]`).
    pub consensus_commit_index: NonZeroU64,
    /// Leader election term at commitment (`>= 1`).
    pub term: NonZeroU64,
    /// Canonical `ControlPlaneStateMachine` root digest after applying up to
    /// `consensus_commit_index`.
    pub state_root_digest: [u8; 32],
    /// Deterministic state-machine application outcome.
    pub outcome: AppliedProposalOutcome,
}

/// Fail-closed persistence, durability, and corruption errors
/// (`[DUR-FAILCLOSED-001]`).
#[derive(Debug)]
pub enum ConsensusStoreError {
    /// Required `WAL` or `synchronous = FULL` mode could not be enabled.
    DurabilityUnavailable(&'static str),
    /// On-disk `SQLite`, WAL, log entry, hard state, or snapshot failed
    /// integrity verification (`[DUR-FAILCLOSED-001]`).
    IntegrityCorrupted(String),
    /// Snapshot creation or log compaction violated `[DUR-SNAP-001]` bounds.
    InvalidSnapshotBoundary(String),
    /// Underlying filesystem I/O failure.
    Io(std::io::Error),
    /// Underlying `SQLite` engine error.
    Sqlite(rusqlite::Error),
}

impl fmt::Display for ConsensusStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DurabilityUnavailable(msg) => {
                write!(
                    formatter,
                    "[DUR-FAILCLOSED-001] durability unavailable: {msg}"
                )
            }
            Self::IntegrityCorrupted(msg) => {
                write!(
                    formatter,
                    "[DUR-FAILCLOSED-001] consensus store integrity corrupted: {msg}"
                )
            }
            Self::InvalidSnapshotBoundary(msg) => {
                write!(
                    formatter,
                    "[DUR-SNAP-001] invalid snapshot/compaction boundary: {msg}"
                )
            }
            Self::Io(err) => write!(formatter, "consensus store I/O error: {err}"),
            Self::Sqlite(err) => write!(formatter, "consensus store SQLite error: {err}"),
        }
    }
}

impl Error for ConsensusStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Sqlite(err) => Some(err),
            Self::DurabilityUnavailable(_)
            | Self::IntegrityCorrupted(_)
            | Self::InvalidSnapshotBoundary(_) => None,
        }
    }
}

impl From<std::io::Error> for ConsensusStoreError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<rusqlite::Error> for ConsensusStoreError {
    fn from(value: rusqlite::Error) -> Self {
        match &value {
            rusqlite::Error::SqliteFailure(code, detail)
                if matches!(
                    code.code,
                    rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
                ) =>
            {
                Self::IntegrityCorrupted(format!("sqlite corrupt ({code:?}): {detail:?}"))
            }
            _ => Self::Sqlite(value),
        }
    }
}

/// Verified recovery bundle returned by [`SqliteConsensusStore::open_or_recover`].
#[derive(Clone, Debug)]
pub struct RecoveredConsensusState {
    /// Verified hard state, snapshot, and contiguous uncompacted log tail for
    /// [`crate::core::ConsensusCore::rehydrate`].
    pub core_state: RehydratedCoreState,
    /// Rehydrated [`ControlPlaneStateMachine`] replayed through `commit_index`.
    pub state_machine: ControlPlaneStateMachine,
}

#[derive(Clone, Debug)]
struct StoredSnapshotRow {
    meta: SnapshotMetadata,
    file_name: String,
    byte_len: u64,
    snapshot_sha256: [u8; 32],
}

/// Durable `SQLite WAL + synchronous=FULL` Raft consensus store.
#[derive(Debug)]
pub struct SqliteConsensusStore {
    store_dir: PathBuf,
    conn: Connection,
}

impl SqliteConsensusStore {
    /// Opens an existing store directory or initializes a fresh one, running
    /// full `[DUR-FAILCLOSED-001]` integrity checks and replaying committed
    /// state (`[DUR-SNAP-001]`).
    ///
    /// # Errors
    ///
    /// Fails closed if `WAL + FULL` cannot be activated or if any `SQLite`
    /// page, WAL header, hard-state digest, log `entry_sha256`, contiguity
    /// invariant, or snapshot digest is corrupted.
    #[allow(clippy::too_many_lines)]
    pub fn open_or_recover(
        store_dir: impl AsRef<Path>,
    ) -> Result<(Self, RecoveredConsensusState), ConsensusStoreError> {
        let store_dir = store_dir.as_ref().to_path_buf();
        fs::create_dir_all(&store_dir)?;
        cleanup_staging_tmp_files(&store_dir)?;

        let db_path = store_dir.join(DB_FILE_NAME);
        verify_wal_header_if_present(&store_dir)?;

        let conn = Connection::open(&db_path)?;
        configure_and_verify_pragmas(&conn)?;
        run_sqlite_quick_check(&conn)?;
        initialize_strict_schema(&conn)?;

        let mut store = Self { store_dir, conn };
        let recovered = store.load_and_verify_all()?;
        Ok((store, recovered))
    }

    /// Directory containing `consensus.sqlite3` and snapshot `.bin` artifacts.
    #[must_use]
    pub fn store_dir(&self) -> &Path {
        &self.store_dir
    }

    /// Forces a `TRUNCATE` WAL checkpoint (useful in deterministic crash and
    /// corruption verification tests).
    ///
    /// # Errors
    ///
    /// Returns [`ConsensusStoreError`] if `SQLite` rejects the checkpoint.
    pub fn checkpoint_wal_truncate(&self) -> Result<(), ConsensusStoreError> {
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE);", [], |_row| Ok(()))?;
        Ok(())
    }

    /// Reads the durable `(current_term, voted_for, commit_index)` tuple,
    /// verifying `hard_state_sha256` (`[DUR-FAILCLOSED-001]`).
    ///
    /// # Errors
    ///
    /// Fails closed if `consensus_meta` is missing or its digest mismatches.
    pub fn read_hard_state(
        &self,
    ) -> Result<(ConsensusTerm, Option<CellIdentity>, LogIndex), ConsensusStoreError> {
        read_and_verify_hard_state_conn(&self.conn)
    }

    /// Atomically persists `(current_term, voted_for)` in `SQLite WAL + FULL`.
    ///
    /// # Errors
    ///
    /// Fails closed on term regression, double-voting in the same term, or I/O
    /// failure.
    pub fn persist_hard_state(
        &mut self,
        current_term: ConsensusTerm,
        voted_for: Option<CellIdentity>,
    ) -> Result<(), ConsensusStoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (_, _, commit_index) = read_and_verify_hard_state_conn(&tx)?;
        write_hard_state_tx(&tx, current_term, voted_for, commit_index)?;
        tx.commit()?;
        Ok(())
    }

    /// Appends contiguous [`RaftLogEntry`] records to `consensus_log` inside a
    /// single `SQLite WAL + FULL` transaction.
    ///
    /// # Errors
    ///
    /// Fails closed if any entry fails integrity verification or violates
    /// `SQLite` constraints.
    pub fn append_entries(&mut self, entries: &[RaftLogEntry]) -> Result<(), ConsensusStoreError> {
        if entries.is_empty() {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        append_entries_tx(&tx, entries)?;
        tx.commit()?;
        Ok(())
    }

    /// Truncates uncommitted conflicting suffix entries at `>= from_index`.
    ///
    /// # Errors
    ///
    /// Fails closed if `from_index <= commit_index` or `SQLite` rejects the
    /// deletion.
    pub fn truncate_log_suffix_from(
        &mut self,
        from_index: LogIndex,
    ) -> Result<usize, ConsensusStoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (_, _, commit_index) = read_and_verify_hard_state_conn(&tx)?;
        if from_index <= commit_index {
            return Err(ConsensusStoreError::IntegrityCorrupted(format!(
                "refusing to truncate committed index {from_index} <= commit_index {commit_index}"
            )));
        }
        let deleted = tx.execute(
            "DELETE FROM consensus_log WHERE log_index >= ?1;",
            params![u64_to_i64(from_index)?],
        )?;
        tx.commit()?;
        Ok(deleted)
    }

    /// Advances durable `commit_index` in `consensus_meta`.
    ///
    /// # Errors
    ///
    /// Fails closed if `commit_index` regresses or exceeds durable log bounds.
    pub fn advance_commit_index(
        &mut self,
        commit_index: LogIndex,
    ) -> Result<(), ConsensusStoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (current_term, voted_for, prev_commit) = read_and_verify_hard_state_conn(&tx)?;
        if commit_index < prev_commit {
            return Err(ConsensusStoreError::IntegrityCorrupted(format!(
                "commit_index regression {commit_index} < {prev_commit}"
            )));
        }
        verify_commit_bound_covered_tx(&tx, commit_index)?;
        write_hard_state_tx(&tx, current_term, voted_for, commit_index)?;
        tx.commit()?;
        Ok(())
    }

    /// Reads and `SHA-256`-verifies a single [`RaftLogEntry`] by `log_index`
    /// (`[DUR-FAILCLOSED-001]`).
    ///
    /// # Errors
    ///
    /// Fails closed with [`ConsensusStoreError::IntegrityCorrupted`] if the
    /// stored row's `entry_sha256` or payload framing is invalid.
    pub fn read_log_entry(
        &self,
        log_index: LogIndex,
    ) -> Result<Option<RaftLogEntry>, ConsensusStoreError> {
        let idx_i64 = u64_to_i64(log_index)?;
        let row = self
            .conn
            .query_row(
                "SELECT log_index, term, command_kind, idempotency_key, payload_bytes, entry_sha256
                 FROM consensus_log WHERE log_index = ?1;",
                params![idx_i64],
                decode_raw_log_row,
            )
            .optional()?;
        row.map(verify_and_hydrate_log_row).transpose()
    }

    /// Reads and `SHA-256`-verifies a contiguous inclusive log slice
    /// `start_inclusive..=end_inclusive` (`[DUR-FAILCLOSED-001]`).
    ///
    /// # Errors
    ///
    /// Fails closed if any entry is corrupted or if a gap exists inside the
    /// returned range.
    pub fn read_log_range(
        &self,
        start_inclusive: LogIndex,
        end_inclusive: LogIndex,
    ) -> Result<Vec<RaftLogEntry>, ConsensusStoreError> {
        if start_inclusive == 0 || end_inclusive < start_inclusive {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT log_index, term, command_kind, idempotency_key, payload_bytes, entry_sha256
             FROM consensus_log
             WHERE log_index >= ?1 AND log_index <= ?2
             ORDER BY log_index ASC;",
        )?;
        let rows = stmt.query_map(
            params![u64_to_i64(start_inclusive)?, u64_to_i64(end_inclusive)?],
            decode_raw_log_row,
        )?;
        let mut out = Vec::new();
        let mut expected = start_inclusive;
        for row_res in rows {
            let entry = verify_and_hydrate_log_row(row_res?)?;
            if entry.index != expected {
                return Err(ConsensusStoreError::IntegrityCorrupted(format!(
                    "non-contiguous log range: expected index {expected}, found {}",
                    entry.index
                )));
            }
            expected = expected.saturating_add(1);
            out.push(entry);
        }
        Ok(out)
    }

    /// Creates and durably commits a local `[DUR-SNAP-001]` snapshot at
    /// `(included_index, included_term)` without deleting log rows yet (caller
    /// may invoke [`Self::compact_log_up_to`] immediately or later).
    ///
    /// # Errors
    ///
    /// Fails closed if `state_bytes` fails snapshot verification, if
    /// `(included_index, included_term)` mismatches `state_bytes`, or if
    /// `included_index > commit_index`.
    pub fn create_snapshot(
        &mut self,
        included_index: LogIndex,
        included_term: ConsensusTerm,
        state_bytes: &[u8],
    ) -> Result<SnapshotMetadata, ConsensusStoreError> {
        if included_index == 0 || included_term == 0 {
            return Err(ConsensusStoreError::InvalidSnapshotBoundary(
                "snapshot included_index and included_term must be >= 1".to_owned(),
            ));
        }
        let decoded_sm = ControlPlaneStateMachine::decode_snapshot(state_bytes).map_err(|err| {
            ConsensusStoreError::IntegrityCorrupted(format!(
                "refusing to persist invalid snapshot payload: {err}"
            ))
        })?;
        if decoded_sm.last_applied_index() != included_index
            || decoded_sm.last_applied_term() != included_term
        {
            return Err(ConsensusStoreError::InvalidSnapshotBoundary(format!(
                "snapshot header ({}, {}) != requested ({included_index}, {included_term})",
                decoded_sm.last_applied_index(),
                decoded_sm.last_applied_term()
            )));
        }

        let state_root_digest = decoded_sm.state_root_digest();
        let snapshot_sha256 = compute_snapshot_transfer_sha256(
            included_index,
            included_term,
            &state_root_digest,
            state_bytes,
        );
        let file_name = snapshot_file_name(included_index, included_term);
        write_atomic_snapshot_file(&self.store_dir, &file_name, state_bytes)?;

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (_, _, commit_index) = read_and_verify_hard_state_conn(&tx)?;
        if included_index > commit_index {
            return Err(ConsensusStoreError::InvalidSnapshotBoundary(format!(
                "cannot snapshot uncommitted index {included_index} > commit_index {commit_index}"
            )));
        }
        if let Some(prev) = read_snapshot_row_conn(&tx)?
            && included_index < prev.meta.included_index
        {
            return Err(ConsensusStoreError::InvalidSnapshotBoundary(format!(
                "snapshot regression {included_index} < {}",
                prev.meta.included_index
            )));
        }
        upsert_snapshot_row_tx(
            &tx,
            included_index,
            included_term,
            &state_root_digest,
            &file_name,
            state_bytes.len() as u64,
            &snapshot_sha256,
        )?;
        tx.commit()?;

        retire_obsolete_snapshot_files(&self.store_dir, &file_name);
        Ok(SnapshotMetadata {
            included_index,
            included_term,
            state_root_digest,
        })
    }

    /// Atomically installs a Leader-transferred `[DUR-SNAP-001]` snapshot,
    /// prunes covered/superseded log rows, and advances `commit_index` to at
    /// least `meta.included_index`.
    ///
    /// # Errors
    ///
    /// Fails closed if `state_bytes` fails integrity verification or `meta`
    /// mismatches the decoded state machine.
    pub fn install_snapshot(
        &mut self,
        meta: SnapshotMetadata,
        state_bytes: &[u8],
    ) -> Result<ControlPlaneStateMachine, ConsensusStoreError> {
        let decoded_sm = ControlPlaneStateMachine::decode_snapshot(state_bytes).map_err(|err| {
            ConsensusStoreError::IntegrityCorrupted(format!(
                "refusing to install corrupt snapshot: {err}"
            ))
        })?;
        if decoded_sm.last_applied_index() != meta.included_index
            || decoded_sm.last_applied_term() != meta.included_term
            || decoded_sm.state_root_digest() != meta.state_root_digest
        {
            return Err(ConsensusStoreError::IntegrityCorrupted(
                "InstallSnapshot metadata mismatched decoded state machine".to_owned(),
            ));
        }

        let snapshot_sha256 = compute_snapshot_transfer_sha256(
            meta.included_index,
            meta.included_term,
            &meta.state_root_digest,
            state_bytes,
        );
        let file_name = snapshot_file_name(meta.included_index, meta.included_term);
        write_atomic_snapshot_file(&self.store_dir, &file_name, state_bytes)?;

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        install_snapshot_in_tx(
            &tx,
            meta,
            &file_name,
            state_bytes.len() as u64,
            &snapshot_sha256,
        )?;
        tx.commit()?;

        retire_obsolete_snapshot_files(&self.store_dir, &file_name);
        Ok(decoded_sm)
    }

    /// Compacts committed `consensus_log` entries at `<= up_to_index`,
    /// strictly requiring a durable `[DUR-SNAP-001]` snapshot covering at least
    /// `up_to_index`.
    ///
    /// # Errors
    ///
    /// Returns [`ConsensusStoreError::InvalidSnapshotBoundary`] if no durable
    /// snapshot covers `up_to_index`.
    pub fn compact_log_up_to(
        &mut self,
        up_to_index: LogIndex,
    ) -> Result<usize, ConsensusStoreError> {
        if up_to_index == 0 {
            return Ok(0);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(snap) = read_snapshot_row_conn(&tx)? else {
            return Err(ConsensusStoreError::InvalidSnapshotBoundary(format!(
                "cannot compact log up to {up_to_index} without a durable snapshot"
            )));
        };
        if up_to_index > snap.meta.included_index {
            return Err(ConsensusStoreError::InvalidSnapshotBoundary(format!(
                "cannot compact log up to {up_to_index} > snapshot.included_index {}",
                snap.meta.included_index
            )));
        }
        let deleted = tx.execute(
            "DELETE FROM consensus_log WHERE log_index <= ?1;",
            params![u64_to_i64(up_to_index)?],
        )?;
        tx.commit()?;
        Ok(deleted)
    }

    /// Executes all durable [`ConsensusEffect`] mutations produced by a single
    /// [`crate::core::ConsensusCore::step`] transition inside **one** atomic
    /// `SQLite WAL + synchronous=FULL` transaction BEFORE any outbound RPC or
    /// client [`ConsensusCommitReceipt`] is emitted (`[DUR-ACK-002]`).
    ///
    /// In-memory `state_machine` is mutated ONLY after `tx.commit()` succeeds!
    ///
    /// # Errors
    ///
    /// Fails closed on any I/O, `SQLite`, or integrity failure without mutating
    /// `state_machine`.
    ///
    /// # Panics
    ///
    /// Never panics: every `InstallSnapshot` effect is pre-staged prior to
    /// entering the `SQLite` transaction loop.
    #[allow(clippy::too_many_lines)]
    pub fn apply_durable_effects(
        &mut self,
        effects: &[ConsensusEffect],
        state_machine: &mut ControlPlaneStateMachine,
    ) -> Result<Vec<(RaftLogEntry, ConsensusCommitReceipt)>, ConsensusStoreError> {
        let has_storage_effects = effects.iter().any(|eff| {
            matches!(
                eff,
                ConsensusEffect::PersistHardState { .. }
                    | ConsensusEffect::InstallSnapshot { .. }
                    | ConsensusEffect::TruncateLogSuffixFrom { .. }
                    | ConsensusEffect::AppendLogEntries(_)
                    | ConsensusEffect::AdvanceCommitIndex { .. }
            )
        });
        if !has_storage_effects {
            return Ok(Vec::new());
        }

        // Pre-stage any `InstallSnapshot` file on disk (`sync_all` + rename)
        // before opening the SQLite write transaction so the DB transaction
        // stays fast and atomic.
        let mut staged_snapshot: Option<(
            SnapshotMetadata,
            String,
            u64,
            [u8; 32],
            ControlPlaneStateMachine,
        )> = None;

        for effect in effects {
            if let ConsensusEffect::InstallSnapshot {
                meta,
                snapshot_bytes,
            } = effect
            {
                let decoded_sm = ControlPlaneStateMachine::decode_snapshot(snapshot_bytes)
                    .map_err(|err| {
                        ConsensusStoreError::IntegrityCorrupted(format!(
                            "InstallSnapshot effect carried invalid snapshot: {err}"
                        ))
                    })?;
                if decoded_sm.last_applied_index() != meta.included_index
                    || decoded_sm.last_applied_term() != meta.included_term
                    || decoded_sm.state_root_digest() != meta.state_root_digest
                {
                    return Err(ConsensusStoreError::IntegrityCorrupted(
                        "InstallSnapshot effect metadata mismatch".to_owned(),
                    ));
                }
                let sha = compute_snapshot_transfer_sha256(
                    meta.included_index,
                    meta.included_term,
                    &meta.state_root_digest,
                    snapshot_bytes,
                );
                let file_name = snapshot_file_name(meta.included_index, meta.included_term);
                write_atomic_snapshot_file(&self.store_dir, &file_name, snapshot_bytes)?;
                staged_snapshot = Some((
                    *meta,
                    file_name,
                    snapshot_bytes.len() as u64,
                    sha,
                    decoded_sm,
                ));
            }
        }

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (mut cur_term, mut cur_voted_for, mut cur_commit) =
            read_and_verify_hard_state_conn(&tx)?;
        let mut hard_state_dirty = false;

        for effect in effects {
            match effect {
                ConsensusEffect::PersistHardState {
                    current_term,
                    voted_for,
                } => {
                    cur_term = *current_term;
                    cur_voted_for = *voted_for;
                    hard_state_dirty = true;
                }
                ConsensusEffect::InstallSnapshot { meta, .. } => {
                    let (_, file_name, byte_len, sha, _) =
                        staged_snapshot.as_ref().expect("pre-staged snapshot");
                    if hard_state_dirty {
                        write_hard_state_tx(&tx, cur_term, cur_voted_for, cur_commit)?;
                        hard_state_dirty = false;
                    }
                    install_snapshot_in_tx(&tx, *meta, file_name, *byte_len, sha)?;
                    cur_commit = cur_commit.max(meta.included_index);
                }
                ConsensusEffect::TruncateLogSuffixFrom { from_index } => {
                    if *from_index <= cur_commit {
                        return Err(ConsensusStoreError::IntegrityCorrupted(format!(
                            "TruncateLogSuffixFrom({from_index}) <= commit_index({cur_commit})"
                        )));
                    }
                    tx.execute(
                        "DELETE FROM consensus_log WHERE log_index >= ?1;",
                        params![u64_to_i64(*from_index)?],
                    )?;
                }
                ConsensusEffect::AppendLogEntries(entries) => {
                    append_entries_tx(&tx, entries)?;
                }
                ConsensusEffect::AdvanceCommitIndex { commit_index, .. } => {
                    if *commit_index < cur_commit {
                        return Err(ConsensusStoreError::IntegrityCorrupted(format!(
                            "AdvanceCommitIndex({commit_index}) < current commit_index({cur_commit})"
                        )));
                    }
                    verify_commit_bound_covered_tx(&tx, *commit_index)?;
                    cur_commit = *commit_index;
                    hard_state_dirty = true;
                }
                ConsensusEffect::LeaderSteppedDown { .. }
                | ConsensusEffect::LinearizableReadConfirmed { .. }
                | ConsensusEffect::ProposalRejected { .. }
                | ConsensusEffect::SendMessage { .. } => {}
            }
        }

        if hard_state_dirty {
            write_hard_state_tx(&tx, cur_term, cur_voted_for, cur_commit)?;
        }
        tx.commit()?;

        // SQLite WAL+FULL commit succeeded! Now deterministically advance the
        // in-memory state machine and build `[DUR-ACK-002]` commit receipts.
        if let Some((_, file_name, _, _, installed_sm)) = staged_snapshot {
            retire_obsolete_snapshot_files(&self.store_dir, &file_name);
            if installed_sm.last_applied_index() >= state_machine.last_applied_index() {
                *state_machine = installed_sm;
            }
        }

        let mut committed_receipts = Vec::new();
        for effect in effects {
            if let ConsensusEffect::AdvanceCommitIndex {
                newly_committed, ..
            } = effect
            {
                for entry in newly_committed {
                    if entry.index <= state_machine.last_applied_index() {
                        continue;
                    }
                    let commit_idx = NonZeroU64::new(entry.index).ok_or_else(|| {
                        ConsensusStoreError::IntegrityCorrupted(
                            "committed entry had zero log_index".to_owned(),
                        )
                    })?;
                    let entry_term = NonZeroU64::new(entry.term).ok_or_else(|| {
                        ConsensusStoreError::IntegrityCorrupted(
                            "committed entry had zero term".to_owned(),
                        )
                    })?;
                    let outcome = state_machine.apply_committed_entry(
                        commit_idx,
                        entry_term,
                        &entry.proposal,
                    );
                    let receipt = ConsensusCommitReceipt {
                        consensus_commit_index: commit_idx,
                        term: entry_term,
                        state_root_digest: state_machine.state_root_digest(),
                        outcome,
                    };
                    committed_receipts.push((*entry, receipt));
                }
            }
        }

        Ok(committed_receipts)
    }

    #[allow(clippy::too_many_lines)]
    fn load_and_verify_all(&mut self) -> Result<RecoveredConsensusState, ConsensusStoreError> {
        let (current_term, voted_for, commit_index) = read_and_verify_hard_state_conn(&self.conn)?;
        let snapshot_row = read_snapshot_row_conn(&self.conn)?;

        let (mut state_machine, loaded_snapshot) = match snapshot_row {
            Some(row) => {
                let snap_path = self.store_dir.join(&row.file_name);
                let bytes = fs::read(&snap_path).map_err(|err| {
                    ConsensusStoreError::IntegrityCorrupted(format!(
                        "missing or unreadable snapshot file {}: {err}",
                        snap_path.display()
                    ))
                })?;
                if bytes.len() as u64 != row.byte_len {
                    return Err(ConsensusStoreError::IntegrityCorrupted(format!(
                        "snapshot {} byte length {} != expected {}",
                        row.file_name,
                        bytes.len(),
                        row.byte_len
                    )));
                }
                let actual_sha = compute_snapshot_transfer_sha256(
                    row.meta.included_index,
                    row.meta.included_term,
                    &row.meta.state_root_digest,
                    &bytes,
                );
                if actual_sha != row.snapshot_sha256 {
                    return Err(ConsensusStoreError::IntegrityCorrupted(format!(
                        "snapshot {} SHA-256 mismatch",
                        row.file_name
                    )));
                }
                let sm = ControlPlaneStateMachine::decode_snapshot(&bytes).map_err(
                    |err: SnapshotCodecError| {
                        ConsensusStoreError::IntegrityCorrupted(format!(
                            "snapshot {} decode failed: {err}",
                            row.file_name
                        ))
                    },
                )?;
                if sm.last_applied_index() != row.meta.included_index
                    || sm.last_applied_term() != row.meta.included_term
                    || sm.state_root_digest() != row.meta.state_root_digest
                {
                    return Err(ConsensusStoreError::IntegrityCorrupted(
                        "snapshot metadata mismatched decoded state machine".to_owned(),
                    ));
                }
                retire_obsolete_snapshot_files(&self.store_dir, &row.file_name);
                (sm, Some((row.meta, bytes)))
            }
            None => (ControlPlaneStateMachine::new(), None),
        };

        let snapshot_included = loaded_snapshot
            .as_ref()
            .map_or(0, |(m, _)| m.included_index);
        if commit_index < snapshot_included {
            return Err(ConsensusStoreError::IntegrityCorrupted(format!(
                "commit_index {commit_index} < snapshot.included_index {snapshot_included}"
            )));
        }

        // Verify EVERY row in `consensus_log` (`[DUR-FAILCLOSED-001]`), even
        // rows `<= snapshot_included` if a crash occurred between
        // `create_snapshot` and `compact_log_up_to`.
        let mut all_rows = Vec::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT log_index, term, command_kind, idempotency_key, payload_bytes, entry_sha256
                 FROM consensus_log ORDER BY log_index ASC;",
            )?;
            let mapped = stmt.query_map([], decode_raw_log_row)?;
            for row_res in mapped {
                all_rows.push(verify_and_hydrate_log_row(row_res?)?);
            }
        }

        let mut log_tail = Vec::new();
        let expected_tail_start = snapshot_included.saturating_add(1);
        let mut expected_next = None::<LogIndex>;

        for entry in all_rows {
            if let Some(exp) = expected_next
                && entry.index != exp
            {
                return Err(ConsensusStoreError::IntegrityCorrupted(format!(
                    "log index gap detected during recovery: expected {exp}, found {}",
                    entry.index
                )));
            }
            expected_next = Some(entry.index.saturating_add(1));

            if entry.index > snapshot_included {
                log_tail.push(entry);
            }
        }

        if let Some(first_tail) = log_tail.first()
            && first_tail.index != expected_tail_start
        {
            return Err(ConsensusStoreError::IntegrityCorrupted(format!(
                "log tail does not start at snapshot_included + 1 ({expected_tail_start}): found {}",
                first_tail.index
            )));
        }

        let last_available_index = log_tail.last().map_or(snapshot_included, |e| e.index);
        if last_available_index < commit_index {
            return Err(ConsensusStoreError::IntegrityCorrupted(format!(
                "durable log ends at {last_available_index} < commit_index {commit_index}"
            )));
        }

        // If a crash occurred after `create_snapshot` committed in SQLite but
        // before `compact_log_up_to` ran, clean up the already-snapshotted
        // prefix now that all entries passed `entry_sha256` verification.
        if snapshot_included > 0 {
            self.conn.execute(
                "DELETE FROM consensus_log WHERE log_index <= ?1;",
                params![u64_to_i64(snapshot_included)?],
            )?;
        }

        // Deterministically replay committed tail entries (`snapshot_included + 1 ..= commit_index`).
        for entry in &log_tail {
            if entry.index > commit_index {
                break;
            }
            let idx_nz = NonZeroU64::new(entry.index).ok_or_else(|| {
                ConsensusStoreError::IntegrityCorrupted("zero log_index in tail".to_owned())
            })?;
            let term_nz = NonZeroU64::new(entry.term).ok_or_else(|| {
                ConsensusStoreError::IntegrityCorrupted("zero term in tail".to_owned())
            })?;
            state_machine.apply_committed_entry(idx_nz, term_nz, &entry.proposal);
        }

        Ok(RecoveredConsensusState {
            core_state: RehydratedCoreState {
                current_term,
                voted_for,
                commit_index,
                snapshot: loaded_snapshot,
                log_tail,
            },
            state_machine,
        })
    }
}

// ============================================================================
// Internal SQLite Schema, Triggers, Checksums & Atomic File Helpers
// ============================================================================

#[derive(Debug)]
struct RawLogRow {
    log_index: i64,
    term: i64,
    command_kind: i64,
    idempotency_key: Vec<u8>,
    payload_bytes: Vec<u8>,
    entry_sha256: Vec<u8>,
}

fn decode_raw_log_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawLogRow> {
    Ok(RawLogRow {
        log_index: row.get(0)?,
        term: row.get(1)?,
        command_kind: row.get(2)?,
        idempotency_key: row.get(3)?,
        payload_bytes: row.get(4)?,
        entry_sha256: row.get(5)?,
    })
}

fn verify_and_hydrate_log_row(raw: RawLogRow) -> Result<RaftLogEntry, ConsensusStoreError> {
    let index = u64::try_from(raw.log_index).map_err(|_| {
        ConsensusStoreError::IntegrityCorrupted(format!("negative log_index {}", raw.log_index))
    })?;
    let term = u64::try_from(raw.term).map_err(|_| {
        ConsensusStoreError::IntegrityCorrupted(format!("negative term {}", raw.term))
    })?;
    if index == 0 || term == 0 {
        return Err(ConsensusStoreError::IntegrityCorrupted(format!(
            "invalid zero index/term ({index}, {term})"
        )));
    }
    let kind_u8 = u8::try_from(raw.command_kind).map_err(|_| {
        ConsensusStoreError::IntegrityCorrupted(format!(
            "out-of-range command_kind {}",
            raw.command_kind
        ))
    })?;
    let family_code = CommandFamilyCode::from_wire_tag(kind_u8).map_err(|reject| {
        ConsensusStoreError::IntegrityCorrupted(format!(
            "forbidden command_kind in consensus_log: {reject}"
        ))
    })?;
    let idem_arr: [u8; 16] = raw.idempotency_key.try_into().map_err(|_| {
        ConsensusStoreError::IntegrityCorrupted("idempotency_key length != 16".to_owned())
    })?;
    let stored_sha: [u8; 32] = raw.entry_sha256.try_into().map_err(|_| {
        ConsensusStoreError::IntegrityCorrupted("entry_sha256 length != 32".to_owned())
    })?;

    let expected_sha = compute_log_entry_sha256(
        index,
        term,
        family_code.as_u8(),
        &idem_arr,
        &raw.payload_bytes,
    );
    if stored_sha != expected_sha {
        return Err(ConsensusStoreError::IntegrityCorrupted(format!(
            "entry_sha256 mismatch at log_index={index}"
        )));
    }

    let proposal = ControlPlaneProposal::decode_command_body(
        family_code,
        IdempotencyKey::from_bytes(idem_arr),
        &raw.payload_bytes,
    )
    .map_err(|err: ScopeViolationError| {
        ConsensusStoreError::IntegrityCorrupted(format!(
            "corrupted command payload at log_index={index}: {err}"
        ))
    })?;

    Ok(RaftLogEntry {
        index,
        term,
        proposal,
        entry_sha256: stored_sha,
    })
}

fn configure_and_verify_pragmas(conn: &Connection) -> Result<(), ConsensusStoreError> {
    conn.busy_timeout(std::time::Duration::from_millis(5_000))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;

    let journal_mode: String =
        conn.query_row("PRAGMA journal_mode = WAL;", [], |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(ConsensusStoreError::DurabilityUnavailable(
            "SQLite refused PRAGMA journal_mode = WAL",
        ));
    }

    conn.pragma_update(None, "synchronous", "FULL")?;
    let sync_level: i64 = conn.query_row("PRAGMA synchronous;", [], |row| row.get(0))?;
    // In SQLite, `2` = FULL and `3` = EXTRA.
    if sync_level < 2 {
        return Err(ConsensusStoreError::DurabilityUnavailable(
            "SQLite refused PRAGMA synchronous = FULL",
        ));
    }

    let fk_enabled: i64 = conn.query_row("PRAGMA foreign_keys;", [], |row| row.get(0))?;
    if fk_enabled != 1 {
        return Err(ConsensusStoreError::DurabilityUnavailable(
            "SQLite refused PRAGMA foreign_keys = ON",
        ));
    }
    Ok(())
}

fn run_sqlite_quick_check(conn: &Connection) -> Result<(), ConsensusStoreError> {
    let mut stmt = conn.prepare("PRAGMA quick_check;")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    for row_res in rows {
        let status = row_res?;
        if !status.eq_ignore_ascii_case("ok") {
            return Err(ConsensusStoreError::IntegrityCorrupted(format!(
                "SQLite PRAGMA quick_check failed: {status}"
            )));
        }
    }
    Ok(())
}

fn verify_wal_header_if_present(store_dir: &Path) -> Result<(), ConsensusStoreError> {
    let wal_path = store_dir.join(format!("{DB_FILE_NAME}-wal"));
    let Ok(bytes) = fs::read(&wal_path) else {
        return Ok(());
    };
    if bytes.is_empty() {
        return Ok(());
    }
    if bytes.len() < 32 {
        return Err(ConsensusStoreError::IntegrityCorrupted(format!(
            "truncated SQLite WAL header ({} bytes < 32)",
            bytes.len()
        )));
    }
    let magic = u32::from_be_bytes(bytes[0..4].try_into().expect("4B"));
    let little_endian_cksum = match magic {
        0x377f_0682 => true,
        0x377f_0683 => false,
        other => {
            return Err(ConsensusStoreError::IntegrityCorrupted(format!(
                "corrupted SQLite WAL magic 0x{other:08x}"
            )));
        }
    };
    let (s0, s1) = sqlite_wal_checksum(&bytes[0..24], 0, 0, little_endian_cksum);
    let expected_s0 = u32::from_be_bytes(bytes[24..28].try_into().expect("4B"));
    let expected_s1 = u32::from_be_bytes(bytes[28..32].try_into().expect("4B"));
    if s0 != expected_s0 || s1 != expected_s1 {
        return Err(ConsensusStoreError::IntegrityCorrupted(
            "SQLite WAL 32-byte header checksum mismatch".to_owned(),
        ));
    }
    Ok(())
}

fn sqlite_wal_checksum(data: &[u8], mut s0: u32, mut s1: u32, little_endian: bool) -> (u32, u32) {
    for pair in data.as_chunks::<8>().0 {
        let first = [pair[0], pair[1], pair[2], pair[3]];
        let second = [pair[4], pair[5], pair[6], pair[7]];
        let w0 = if little_endian {
            u32::from_le_bytes(first)
        } else {
            u32::from_be_bytes(first)
        };
        let w1 = if little_endian {
            u32::from_le_bytes(second)
        } else {
            u32::from_be_bytes(second)
        };
        s0 = s0.wrapping_add(w0).wrapping_add(s1);
        s1 = s1.wrapping_add(w1).wrapping_add(s0);
    }
    (s0, s1)
}

fn initialize_strict_schema(conn: &Connection) -> Result<(), ConsensusStoreError> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS consensus_meta (
            singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
            current_term INTEGER NOT NULL CHECK (current_term >= 0),
            voted_for BLOB CHECK (voted_for IS NULL OR length(voted_for) = 16),
            commit_index INTEGER NOT NULL CHECK (commit_index >= 0),
            hard_state_sha256 BLOB NOT NULL CHECK (length(hard_state_sha256) = 32)
        ) STRICT;

        CREATE TRIGGER IF NOT EXISTS trg_consensus_meta_monotonic
        BEFORE UPDATE ON consensus_meta
        BEGIN
            SELECT CASE
                WHEN NEW.current_term < OLD.current_term THEN
                    RAISE(ABORT, 'consensus_meta.current_term regression forbidden')
                WHEN NEW.commit_index < OLD.commit_index THEN
                    RAISE(ABORT, 'consensus_meta.commit_index regression forbidden')
                WHEN NEW.current_term = OLD.current_term
                     AND OLD.voted_for IS NOT NULL
                     AND (NEW.voted_for IS NULL OR NEW.voted_for != OLD.voted_for) THEN
                    RAISE(ABORT, 'single-vote-per-term violation in consensus_meta')
            END;
        END;

        CREATE TRIGGER IF NOT EXISTS trg_consensus_meta_no_delete
        BEFORE DELETE ON consensus_meta
        BEGIN
            SELECT RAISE(ABORT, 'consensus_meta singleton cannot be deleted');
        END;

        CREATE TABLE IF NOT EXISTS consensus_log (
            log_index INTEGER PRIMARY KEY CHECK (log_index >= 1),
            term INTEGER NOT NULL CHECK (term >= 1),
            command_kind INTEGER NOT NULL CHECK (command_kind IN (1, 2, 3, 4)),
            idempotency_key BLOB NOT NULL CHECK (length(idempotency_key) = 16),
            payload_bytes BLOB NOT NULL,
            entry_sha256 BLOB NOT NULL CHECK (length(entry_sha256) = 32)
        ) STRICT;

        CREATE TRIGGER IF NOT EXISTS trg_consensus_log_no_update
        BEFORE UPDATE ON consensus_log
        BEGIN
            SELECT RAISE(ABORT, 'consensus_log rows are immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_consensus_log_safe_delete
        BEFORE DELETE ON consensus_log
        WHEN OLD.log_index <= (SELECT commit_index FROM consensus_meta WHERE singleton = 1)
         AND OLD.log_index > COALESCE((SELECT included_index FROM consensus_snapshot WHERE singleton = 1), 0)
        BEGIN
            SELECT RAISE(ABORT, 'cannot delete committed log entry not covered by durable snapshot');
        END;

        CREATE TABLE IF NOT EXISTS consensus_snapshot (
            singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
            included_index INTEGER NOT NULL CHECK (included_index >= 1),
            included_term INTEGER NOT NULL CHECK (included_term >= 1),
            state_root_digest BLOB NOT NULL CHECK (length(state_root_digest) = 32),
            snapshot_file_name TEXT NOT NULL CHECK (length(snapshot_file_name) > 0),
            snapshot_byte_len INTEGER NOT NULL CHECK (snapshot_byte_len >= 76),
            snapshot_sha256 BLOB NOT NULL CHECK (length(snapshot_sha256) = 32)
        ) STRICT;

        CREATE TRIGGER IF NOT EXISTS trg_consensus_snapshot_monotonic
        BEFORE UPDATE ON consensus_snapshot
        WHEN NEW.included_index < OLD.included_index
        BEGIN
            SELECT RAISE(ABORT, 'consensus_snapshot.included_index regression forbidden');
        END;
        ",
    )?;

    let existing_meta: Option<i64> = conn
        .query_row(
            "SELECT singleton FROM consensus_meta WHERE singleton = 1;",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if existing_meta.is_none() {
        let genesis_sha = compute_hard_state_sha256(0, None, 0);
        conn.execute(
            "INSERT INTO consensus_meta (singleton, current_term, voted_for, commit_index, hard_state_sha256)
             VALUES (1, 0, NULL, 0, ?1);",
            params![genesis_sha.as_slice()],
        )?;
    }
    Ok(())
}

fn compute_hard_state_sha256(
    current_term: ConsensusTerm,
    voted_for: Option<CellIdentity>,
    commit_index: LogIndex,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(HARD_STATE_HASH_DOMAIN);
    hasher.update(current_term.to_be_bytes());
    match voted_for {
        Some(cell) => {
            hasher.update([1u8]);
            hasher.update(cell.as_bytes());
        }
        None => hasher.update([0u8]),
    }
    hasher.update(commit_index.to_be_bytes());
    hasher.finalize().into()
}

type RawHardStateRow = (i64, Option<Vec<u8>>, i64, Vec<u8>);

fn read_and_verify_hard_state_conn(
    conn: &Connection,
) -> Result<(ConsensusTerm, Option<CellIdentity>, LogIndex), ConsensusStoreError> {
    let row: Option<RawHardStateRow> = conn
        .query_row(
            "SELECT current_term, voted_for, commit_index, hard_state_sha256
             FROM consensus_meta WHERE singleton = 1;",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((term_i64, voted_blob, commit_i64, sha_blob)) = row else {
        return Err(ConsensusStoreError::IntegrityCorrupted(
            "missing consensus_meta singleton row".to_owned(),
        ));
    };

    let current_term = u64::try_from(term_i64).map_err(|_| {
        ConsensusStoreError::IntegrityCorrupted(format!("negative current_term {term_i64}"))
    })?;
    let commit_index = u64::try_from(commit_i64).map_err(|_| {
        ConsensusStoreError::IntegrityCorrupted(format!("negative commit_index {commit_i64}"))
    })?;
    let voted_for = match voted_blob {
        Some(bytes) => {
            let arr: [u8; 16] = bytes.try_into().map_err(|_| {
                ConsensusStoreError::IntegrityCorrupted("voted_for length != 16".to_owned())
            })?;
            if arr == [0u8; 16] {
                return Err(ConsensusStoreError::IntegrityCorrupted(
                    "all-zero voted_for".to_owned(),
                ));
            }
            Some(CellIdentity::from_domain(SchedulerDomainId::from_bytes(
                arr,
            )))
        }
        None => None,
    };
    let stored_sha: [u8; 32] = sha_blob.try_into().map_err(|_| {
        ConsensusStoreError::IntegrityCorrupted("hard_state_sha256 length != 32".to_owned())
    })?;

    let expected_sha = compute_hard_state_sha256(current_term, voted_for, commit_index);
    if stored_sha != expected_sha {
        return Err(ConsensusStoreError::IntegrityCorrupted(
            "consensus_meta hard_state_sha256 mismatch".to_owned(),
        ));
    }
    Ok((current_term, voted_for, commit_index))
}

fn write_hard_state_tx(
    tx: &Transaction<'_>,
    current_term: ConsensusTerm,
    voted_for: Option<CellIdentity>,
    commit_index: LogIndex,
) -> Result<(), ConsensusStoreError> {
    let sha = compute_hard_state_sha256(current_term, voted_for, commit_index);
    let voted_slice = voted_for.map(|c| c.as_bytes().to_vec());
    tx.execute(
        "UPDATE consensus_meta
         SET current_term = ?1, voted_for = ?2, commit_index = ?3, hard_state_sha256 = ?4
         WHERE singleton = 1;",
        params![
            u64_to_i64(current_term)?,
            voted_slice,
            u64_to_i64(commit_index)?,
            sha.as_slice()
        ],
    )?;
    Ok(())
}

fn append_entries_tx(
    tx: &Transaction<'_>,
    entries: &[RaftLogEntry],
) -> Result<(), ConsensusStoreError> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut stmt = tx.prepare_cached(
        "INSERT INTO consensus_log
         (log_index, term, command_kind, idempotency_key, payload_bytes, entry_sha256)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6);",
    )?;
    for entry in entries {
        if !entry.verify_integrity() {
            return Err(ConsensusStoreError::IntegrityCorrupted(format!(
                "refusing to append entry with invalid entry_sha256 at index={}",
                entry.index
            )));
        }
        let payload = entry.proposal.encode_command_body();
        stmt.execute(params![
            u64_to_i64(entry.index)?,
            u64_to_i64(entry.term)?,
            i64::from(entry.proposal.command.family_code().as_u8()),
            entry.proposal.idempotency_key.as_bytes().as_slice(),
            payload.as_slice(),
            entry.entry_sha256.as_slice(),
        ])?;
    }
    Ok(())
}

fn verify_commit_bound_covered_tx(
    tx: &Transaction<'_>,
    commit_index: LogIndex,
) -> Result<(), ConsensusStoreError> {
    if commit_index == 0 {
        return Ok(());
    }
    let snap_idx = read_snapshot_row_conn(tx)?.map_or(0, |s| s.meta.included_index);
    if commit_index <= snap_idx {
        return Ok(());
    }
    let max_log_opt: Option<i64> =
        tx.query_row("SELECT MAX(log_index) FROM consensus_log;", [], |row| {
            row.get(0)
        })?;
    let max_log = match max_log_opt {
        Some(v) => u64::try_from(v).unwrap_or(0),
        None => 0,
    };
    if max_log < commit_index {
        return Err(ConsensusStoreError::IntegrityCorrupted(format!(
            "cannot advance commit_index to {commit_index}: durable max_log_index is {max_log}"
        )));
    }
    Ok(())
}

type RawSnapshotDbRow = (i64, i64, Vec<u8>, String, i64, Vec<u8>);

fn read_snapshot_row_conn(
    conn: &Connection,
) -> Result<Option<StoredSnapshotRow>, ConsensusStoreError> {
    let raw: Option<RawSnapshotDbRow> = conn
        .query_row(
            "SELECT included_index, included_term, state_root_digest, snapshot_file_name,
                    snapshot_byte_len, snapshot_sha256
             FROM consensus_snapshot WHERE singleton = 1;",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((idx_i64, term_i64, root_blob, file_name, len_i64, sha_blob)) = raw else {
        return Ok(None);
    };
    let included_index = u64::try_from(idx_i64).map_err(|_| {
        ConsensusStoreError::IntegrityCorrupted(format!("negative snapshot index {idx_i64}"))
    })?;
    let included_term = u64::try_from(term_i64).map_err(|_| {
        ConsensusStoreError::IntegrityCorrupted(format!("negative snapshot term {term_i64}"))
    })?;
    let byte_len = u64::try_from(len_i64).map_err(|_| {
        ConsensusStoreError::IntegrityCorrupted(format!("negative snapshot len {len_i64}"))
    })?;
    let state_root_digest: [u8; 32] = root_blob.try_into().map_err(|_| {
        ConsensusStoreError::IntegrityCorrupted("snapshot state_root_digest len != 32".to_owned())
    })?;
    let snapshot_sha256: [u8; 32] = sha_blob.try_into().map_err(|_| {
        ConsensusStoreError::IntegrityCorrupted("snapshot_sha256 len != 32".to_owned())
    })?;

    Ok(Some(StoredSnapshotRow {
        meta: SnapshotMetadata {
            included_index,
            included_term,
            state_root_digest,
        },
        file_name,
        byte_len,
        snapshot_sha256,
    }))
}

fn upsert_snapshot_row_tx(
    tx: &Transaction<'_>,
    included_index: LogIndex,
    included_term: ConsensusTerm,
    state_root_digest: &[u8; 32],
    file_name: &str,
    byte_len: u64,
    snapshot_sha256: &[u8; 32],
) -> Result<(), ConsensusStoreError> {
    tx.execute(
        "INSERT INTO consensus_snapshot
         (singleton, included_index, included_term, state_root_digest,
          snapshot_file_name, snapshot_byte_len, snapshot_sha256)
         VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(singleton) DO UPDATE SET
             included_index = excluded.included_index,
             included_term = excluded.included_term,
             state_root_digest = excluded.state_root_digest,
             snapshot_file_name = excluded.snapshot_file_name,
             snapshot_byte_len = excluded.snapshot_byte_len,
             snapshot_sha256 = excluded.snapshot_sha256;",
        params![
            u64_to_i64(included_index)?,
            u64_to_i64(included_term)?,
            state_root_digest.as_slice(),
            file_name,
            u64_to_i64(byte_len)?,
            snapshot_sha256.as_slice(),
        ],
    )?;
    Ok(())
}

fn install_snapshot_in_tx(
    tx: &Transaction<'_>,
    meta: SnapshotMetadata,
    file_name: &str,
    byte_len: u64,
    snapshot_sha256: &[u8; 32],
) -> Result<(), ConsensusStoreError> {
    let (cur_term, cur_voted_for, cur_commit) = read_and_verify_hard_state_conn(tx)?;

    // Record snapshot row FIRST so `trg_consensus_log_safe_delete` sees all
    // covered entries `<= meta.included_index` as snapshot-backed!
    upsert_snapshot_row_tx(
        tx,
        meta.included_index,
        meta.included_term,
        &meta.state_root_digest,
        file_name,
        byte_len,
        snapshot_sha256,
    )?;

    // Check whether an existing entry at `meta.included_index` has matching
    // `meta.included_term`. If so, retain suffix `> meta.included_index`;
    // otherwise delete the entire stale uncommitted log BEFORE bumping
    // `commit_index`!
    let matching_suffix_anchor: Option<i64> = tx
        .query_row(
            "SELECT term FROM consensus_log WHERE log_index = ?1;",
            params![u64_to_i64(meta.included_index)?],
            |r| r.get(0),
        )
        .optional()?;
    let anchor_matches = matching_suffix_anchor
        .and_then(|t| u64::try_from(t).ok())
        .is_some_and(|t| t == meta.included_term);

    if anchor_matches {
        tx.execute(
            "DELETE FROM consensus_log WHERE log_index <= ?1;",
            params![u64_to_i64(meta.included_index)?],
        )?;
    } else {
        if cur_commit > meta.included_index {
            return Err(ConsensusStoreError::IntegrityCorrupted(format!(
                "refusing InstallSnapshot at {} < commit_index {cur_commit}",
                meta.included_index
            )));
        }
        tx.execute("DELETE FROM consensus_log;", [])?;
    }

    let next_commit = cur_commit.max(meta.included_index);
    let next_term = cur_term.max(meta.included_term);
    let next_voted = if next_term > cur_term {
        None
    } else {
        cur_voted_for
    };
    write_hard_state_tx(tx, next_term, next_voted, next_commit)?;
    Ok(())
}

fn snapshot_file_name(included_index: LogIndex, included_term: ConsensusTerm) -> String {
    format!("snapshot-{included_index:020}-{included_term:020}.bin")
}

fn write_atomic_snapshot_file(
    store_dir: &Path,
    final_file_name: &str,
    bytes: &[u8],
) -> Result<(), ConsensusStoreError> {
    let tmp_path = store_dir.join(format!("{final_file_name}.tmp"));
    let final_path = store_dir.join(final_file_name);
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

fn cleanup_staging_tmp_files(store_dir: &Path) -> Result<(), ConsensusStoreError> {
    for entry_res in fs::read_dir(store_dir)? {
        let entry = entry_res?;
        let name = entry.file_name();
        if let Some(s) = name.to_str()
            && s.starts_with("snapshot-")
            && Path::new(s)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("tmp"))
        {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}

fn retire_obsolete_snapshot_files(store_dir: &Path, active_file_name: &str) {
    let Ok(entries) = fs::read_dir(store_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if let Some(s) = name.to_str()
            && s.starts_with("snapshot-")
            && Path::new(s)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("bin"))
            && s != active_file_name
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn u64_to_i64(value: u64) -> Result<i64, ConsensusStoreError> {
    i64::try_from(value).map_err(|_| {
        ConsensusStoreError::IntegrityCorrupted(format!(
            "u64 value {value} exceeds SQLite INTEGER i64::MAX"
        ))
    })
}
