//! Reconciliation convergence core (CS0-C) — the bounded convergence of the
//! commit-prefix gap between the takeover-completion point and the old
//! authority's last checkpoint, ADR-0019 R1.2.4's consumer half of the
//! checkpoint mechanism face.
//!
//! **What this module is.** The minimal executor `federation.rs` defers to
//! (its module documentation hands "acting on a verified checkpoint during
//! reconciliation" to this slice): it reads one old-authority Cell's
//! checkpoint trail through the CS0-B verifier, derives the old authority's
//! durable commit prefix through the `nlos-task` store's own durable read
//! faces, and converges the uncovered gap to one typed final state. The
//! contract invariants are carried over verbatim from ADR-0013 decision 1
//! (its cross-Cell extension, R1.2.4: "契约不变量照搬……不新增、不放松"):
//! crash-window convergence, no phantom rows, no double commit, and
//! byte-identical replay. Nothing here adds, relaxes, or re-implements a
//! single-machine convergence rule.
//!
//! **Reuse, not duplication.** Every durable fact comes from an existing
//! public face: the prefix entries from
//! [`SqliteTaskAuthority::list_effect_history`], the prefix root from
//! [`SqliteTaskAuthority::compute_effect_history_root`] cross-checked
//! against [`effect_history_root_of`], the head facts from
//! [`SqliteTaskAuthority::inspect_task`], and the term precondition from
//! [`SqliteTaskAuthority::inspect_authority_assignment`]. The trail side is
//! the CS0-B face ([`CellDirectory::checkpoints`],
//! [`CheckpointFact::verify_digest`]). The per-owner convergence machinery
//! of the single-machine half (`converge_resource_commit_plan`, the
//! quarantine/adoption lifecycle) stays where it is and stays out of scope
//! here: this slice's declared endpoint coverage is the `TaskStore`
//! authority only, exactly like the CS0-A drive's.
//!
//! **Durable-prefix grammar (the executor fixes it, authors share it).** A
//! [`CheckpointPrefix`] carries opaque entries whose meaning is "fixed by
//! the authority that computes or checks the digest" (CS0-B). This module
//! is that authority for the task store: one entry is the canonical
//! domain-separated encoding of one durable [`EffectHistoryEntry`], and the
//! checkpoint author — the old Cell in the dual-process evidence, a harness
//! in the single-process scenarios, the successor under its own term later
//! — derives its claim through the same public function
//! ([`durable_commit_prefix`]), so author and verifier can never drift
//! apart.
//!
//! **Term precondition (the dual-primary guard).** Convergence runs as the
//! term holder: the store's active assignment for the task must be `Active`
//! and bound to the presented lease's binding (schema v27/v37 durable
//! facts, read back fail-closed like every CS0-A readback). A stale-term
//! Cell — the still-live old authority after a takeover — is refused with
//! [`ReconciliationError::NotTermHolder`] before any trail or prefix fact
//! is read. The executor deliberately draws no clock: lease *liveness*
//! stays the caller's sequencing obligation (as in the CS0-A drive), which
//! keeps convergence a pure function of its inputs and makes replay
//! byte-idempotency structural rather than aspirational.
//!
//! **What this module is not.** It is not a full executor, not
//! `C-MIGRATE`, and not a network face: the Phase 0 transport shape is the
//! same single-host shared-filesystem ADR-0018 topology as the CS0-A
//! drive (the term holder opens the old authority's store and the shared
//! federation root directly). It does not touch the Cell epoch — the only
//! epoch-advance entry stays the quota family's
//! `advance_epoch_and_quarantine`. Evidence produced through this core is
//! single-host dual-process evidence: citing it as cross-host,
//! network-partition, or cross-clock evidence would violate RISK-B-11.
//!
//! **Visibility honesty (R1.2.3).** The trail alone never licenses a
//! "global now" read: a free-text digest covers nothing (the honest
//! `PARTIAL` / `UNCERTAIN` window), and the outcome's
//! [`PrefixVisibility`] says which window the verified trail leaves
//! uncovered. Convergence itself is unaffected — the final state derives
//! from the durable prefix, never from the trail — which is exactly the
//! bounded-convergence shape R1.2.4 fixes.

use std::error::Error;
use std::fmt;

use nlos_cell::{
    CellDirectory, CellIdentity, CheckpointDigest, CheckpointDigestVerification, CheckpointPrefix,
    CheckpointRecord, FederationError,
};
use nlos_task::{
    AuthorityAssignmentState, AuthorityLeaseRecord, EffectHistoryEntry, SqliteTaskAuthority,
    TaskStoreError, effect_history_root_of,
};
use nlos_types::{ProcessId, TaskId};
use sha2::{Digest, Sha256};

/// Domain-separation string of one durable-prefix entry's canonical
/// encoding — the grammar every checkpoint digest over the task authority's
/// durable prefix is taken on.
const PREFIX_ENTRY_DOMAIN: &[u8] = b"llmos/task-authority-durable-prefix-entry/v1";
/// Domain-separation string of one adopted gap entry's digest inside the
/// convergence record.
const GAP_ENTRY_DOMAIN: &[u8] = b"llmos/reconciliation-gap-entry/v1";
/// Domain-separation string of the canonical convergence-record replay
/// digest — the byte-exact replay discipline's comparison anchor.
const CONVERGENCE_RECORD_DOMAIN: &[u8] = b"llmos/reconciliation-convergence-record/v1";

/// The old authority's durable commit prefix as this module fixes it: the
/// task's durable effect history (strictly increasing
/// `effect_history_seq`, no gaps — the store's own
/// `[TASK-EFFECT-ID-001]` discipline), each entry canonically encoded for
/// the [`CheckpointPrefix`] grammar, plus the durable root and head facts
/// the convergence record anchors on.
///
/// This is the single source of truth for both sides of a checkpoint
/// claim: the author derives its digest from [`Self::prefix`], the
/// executor re-derives and verifies against the same bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableCommitPrefix {
    prefix: CheckpointPrefix,
    entry_count: usize,
    durable_prefix_root: [u8; 32],
    head_commit_seq: u64,
    head_effect_history_root: [u8; 32],
}

impl DurableCommitPrefix {
    /// The canonical prefix entries, oldest first — the bytes every
    /// checkpoint digest over this authority's durable prefix is taken on.
    #[must_use]
    pub fn prefix(&self) -> &CheckpointPrefix {
        &self.prefix
    }

    /// How many durable entries the prefix holds.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.entry_count
    }

    /// Root the store itself recomputes from the durable history rows (the
    /// executor's no-phantom cross-check pins this against the in-memory
    /// formula over the listed entries).
    #[must_use]
    pub fn durable_prefix_root(&self) -> [u8; 32] {
        self.durable_prefix_root
    }

    /// The durable `TaskHead` commit count at derivation time.
    #[must_use]
    pub fn head_commit_seq(&self) -> u64 {
        self.head_commit_seq
    }

    /// The durable `TaskHead` effect-history root at derivation time (the
    /// root as of the last commit; mid-attempt outcomes can extend the
    /// recomputed root beyond it by design).
    #[must_use]
    pub fn head_effect_history_root(&self) -> [u8; 32] {
        self.head_effect_history_root
    }
}

/// Inputs of one convergence run. The store and federation handles are
/// opened by the caller — the Phase 0 shape has the term holder open the
/// old authority's task store and the shared federation root directly off
/// the shared filesystem (the CS0-A transport shape).
pub struct ReconciliationRequest<'a> {
    /// The durable task authority whose commit-prefix gap is converged.
    /// Phase 0: the old authority's store, opened by the term holder.
    pub store: &'a SqliteTaskAuthority,
    /// Federation face the old authority Cell's checkpoint trail is read
    /// through.
    pub directory: &'a CellDirectory,
    /// The old authority Cell whose audit trail is enumerated.
    pub authority_cell: CellIdentity,
    /// Task whose authority prefix is converged.
    pub task_id: TaskId,
    /// The convergence runner's authority lease. Fail-closed precondition:
    /// the store's active assignment for the task must be bound to this
    /// lease's binding (term identity, not liveness — the executor draws no
    /// clock; keeping the lease inside its window is the caller's
    /// sequencing obligation, exactly as in the CS0-A drive).
    pub term_lease: AuthorityLeaseRecord,
}

/// How one trail record's digest axis reads against the durable prefix:
/// either a verified anchor at an exact prefix length, or author free text
/// that covers nothing (R1.2.3's `PARTIAL` / `UNCERTAIN` window).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TrailCover {
    /// [`CheckpointDigestVerification::Verified`] at exactly this prefix
    /// length: the record's claim is derivable from the authority's own
    /// durable prefix, and the first `covered_entries` entries are
    /// trail-covered.
    Anchored {
        /// Prefix length the verified digest pins.
        covered_entries: usize,
    },
    /// No structured-digest tag on the axis: comparable to nothing. The
    /// record is enumerated and reported, never counted as coverage.
    FreeText,
}

/// One trail record plus its cover disposition — the per-record evidence
/// the outcome carries for assertion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrailReview {
    /// The enumerated record, verbatim.
    pub record: CheckpointRecord,
    /// How its digest axis read against the durable prefix.
    pub cover: TrailCover,
}

/// The verified anchor the convergence started from: the trail record whose
/// verified digest pins the longest covered prefix (earliest record on
/// ties, so the choice is a pure function of the trail).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerifiedAnchor {
    /// The anchoring record's wall-recording time (a log axis).
    pub recorded_ms: u64,
    /// OS process that recorded the anchor (a log axis).
    pub os_process_id: u32,
    /// Process-unique sequence of the anchor record (a log axis).
    pub sequence: u64,
    /// Prefix length the anchor's verified digest pins.
    pub covered_entries: usize,
    /// The verified structured digest itself.
    pub digest: CheckpointDigest,
}

/// One gap entry adopted by the convergence: its durable sequence and its
/// domain-separated entry digest. Adoption is bookkeeping, never a
/// re-execution — the entry is already durable in the authority's store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdoptedEntry {
    /// `effect_history_seq` of the adopted durable entry.
    pub effect_history_seq: u64,
    /// Domain-separated digest of the adopted entry's canonical bytes.
    pub entry_digest: [u8; 32],
}

/// The honest visibility declaration of the trail read (R1.2.3): what the
/// verified trail leaves uncovered relative to the durable prefix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrefixVisibility {
    /// No verified anchor at all — the entire durable prefix is the
    /// uncovered window. Convergence still proceeds from the durable
    /// prefix; the trail licenses no coverage claim.
    Uncertain,
    /// A verified anchor exists but covers less than the durable prefix —
    /// the takeover crash window leaves an uncovered tail, which this
    /// convergence adopts.
    Partial,
    /// The verified anchor covers the whole durable prefix — no uncovered
    /// window remains at convergence time.
    Covered,
}

/// The unique final authority state the gap converged to, as the store's
/// own durable facts state it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FinalAuthorityState {
    /// Entries in the final durable prefix.
    pub entry_count: usize,
    /// Root the store recomputes from the durable history rows.
    pub durable_prefix_root: [u8; 32],
    /// Durable `TaskHead` commit count.
    pub head_commit_seq: u64,
    /// Durable `TaskHead` effect-history root (as of the last commit).
    pub head_effect_history_root: [u8; 32],
}

/// The four ADR-0013 decision 1 contract invariants, carried over verbatim
/// by ADR-0019 R1.2.4, each with its computed check on the outcome (a
/// violation is a typed fail-closed error, never a smoothed-over `false`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConvergenceInvariants {
    /// Crash-window convergence: the anchor re-verifies over exactly the
    /// covered prefix bytes, and covered plus adopted lengths compose
    /// exactly the final length.
    pub crash_window_converged: bool,
    /// No phantom rows: the final prefix is the store's own durable
    /// history — contiguous from sequence 1, with the store's recomputed
    /// root equal to the in-memory formula over the listed entries.
    pub no_phantom_rows: bool,
    /// No double commit: no duplicate durable sequences, and every adopted
    /// entry lies strictly beyond the anchor boundary (trail-covered
    /// entries are never re-adopted by this convergence).
    pub no_double_commit: bool,
    /// Replay byte-idempotency anchor: the domain-separated digest over
    /// the record's canonical bytes. Two convergences over the same inputs
    /// must compare equal here (and structurally) — the discipline the
    /// evidence tests assert and CS0-D consumes.
    pub replay_digest: [u8; 32],
}

/// Typed result of one convergence run: the term it ran under, the trail
/// review, the verified anchor, the adopted gap, the final authority
/// state, and the four contract invariants. A pure function of its inputs
/// — no clock, no random, no process identity is drawn — so an identical
/// rerun yields a byte-identical record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconciliationOutcome {
    /// The old authority Cell whose trail was read.
    pub authority_cell: CellIdentity,
    /// The task whose prefix was converged.
    pub task_id: TaskId,
    /// Term the convergence ran under (the presented lease's).
    pub term: u64,
    /// Holder the convergence ran under (the presented lease's).
    pub holder_id: ProcessId,
    /// The verified anchor the gap starts after (`None` when the trail
    /// verifies nothing — the whole prefix is the gap).
    pub anchor: Option<VerifiedAnchor>,
    /// Every enumerated trail record with its cover disposition.
    pub trail: Vec<TrailReview>,
    /// Honest trail visibility relative to the durable prefix.
    pub visibility: PrefixVisibility,
    /// Prefix length the verified trail covers.
    pub covered_entries: usize,
    /// The adopted gap: durable entries beyond the covered prefix, oldest
    /// first (empty when the trail covers everything).
    pub adopted: Vec<AdoptedEntry>,
    /// The unique final authority state.
    pub final_state: FinalAuthorityState,
    /// The four contract invariants' computed checks.
    pub invariants: ConvergenceInvariants,
}

/// Fail-closed errors of the convergence core. Store and federation
/// failures surface verbatim; the remaining variants name the core's own
/// preconditions and integrity checks.
#[derive(Debug)]
pub enum ReconciliationError {
    /// A durable read refused the convergence; carried verbatim.
    Store(TaskStoreError),
    /// The federation face refused the trail enumeration; carried
    /// verbatim.
    Federation(FederationError),
    /// The task has no current authority assignment to converge under.
    NoActiveAssignment,
    /// The presented lease does not bind the store's active assignment —
    /// a stale-term Cell (the dual-primary shape) or a foreign lease.
    /// Refused before any trail or prefix fact is read.
    NotTermHolder {
        /// Term the caller presented.
        presented_term: u64,
        /// Term the store's active assignment is bound to.
        active_term: u64,
    },
    /// The old Cell's trail directory holds unparseable (or misfiled)
    /// checkpoint files — fail-closed, never smoothed into partial
    /// coverage.
    TrailFilesCorrupt {
        /// Corrupt-file count the enumeration reported.
        count: usize,
    },
    /// A trail record tags its digest axis as structured but the body does
    /// not decode (wrong length, non-hex, non-canonical casing). Fail
    /// closed; never silently re-read as free text.
    TrailDigestCorrupt {
        /// The record's wall-recording time (a log axis).
        recorded_ms: u64,
        /// Recording OS process (a log axis).
        os_process_id: u32,
        /// Process-unique sequence (a log axis).
        sequence: u64,
    },
    /// A trail record's well-formed structured digest is derivable from no
    /// prefix of the authority's durable prefix — a tampered claim, a
    /// claim over another store, or a claim over commits the authority
    /// never durably made (the phantom-row refusal, ADR-0013 invariant 2).
    TrailClaimNotDerivable {
        /// The record's wall-recording time (a log axis).
        recorded_ms: u64,
        /// Recording OS process (a log axis).
        os_process_id: u32,
        /// Process-unique sequence (a log axis).
        sequence: u64,
        /// The digest the record presented.
        presented: CheckpointDigest,
        /// The digest recomputed from the full durable prefix at the
        /// record's own fence axes.
        recomputed: CheckpointDigest,
    },
    /// The durable prefix itself failed a structural check (non-contiguous
    /// sequence, or the store's recomputed root diverging from the
    /// in-memory formula over the listed entries). Fail closed: a
    /// converged record is never built on a prefix the store cannot
    /// re-derive.
    InconsistentDurablePrefix {
        /// Which structural check failed.
        reason: &'static str,
    },
}

impl fmt::Display for ReconciliationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "task authority read: {error}"),
            Self::Federation(error) => write!(formatter, "federation trail read: {error}"),
            Self::NoActiveAssignment => {
                write!(formatter, "task has no current authority assignment")
            }
            Self::NotTermHolder {
                presented_term,
                active_term,
            } => write!(
                formatter,
                "convergence lease (term {presented_term}) does not bind the active assignment (term {active_term})"
            ),
            Self::TrailFilesCorrupt { count } => write!(
                formatter,
                "authority Cell's checkpoint trail holds {count} corrupt file(s)"
            ),
            Self::TrailDigestCorrupt {
                recorded_ms,
                os_process_id,
                sequence,
            } => write!(
                formatter,
                "trail record ({recorded_ms}, {os_process_id}, {sequence}) carries a corrupt structured digest body"
            ),
            Self::TrailClaimNotDerivable {
                recorded_ms,
                os_process_id,
                sequence,
                presented,
                recomputed,
            } => write!(
                formatter,
                "trail record ({recorded_ms}, {os_process_id}, {sequence}) claims a digest derivable from no durable prefix: presented {}, recomputed {}",
                presented.to_text(),
                recomputed.to_text()
            ),
            Self::InconsistentDurablePrefix { reason } => write!(
                formatter,
                "durable commit prefix failed its structural check: {reason}"
            ),
        }
    }
}

impl Error for ReconciliationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::Federation(error) => Some(error),
            _ => None,
        }
    }
}

impl From<TaskStoreError> for ReconciliationError {
    fn from(error: TaskStoreError) -> Self {
        Self::Store(error)
    }
}

impl From<FederationError> for ReconciliationError {
    fn from(error: FederationError) -> Self {
        Self::Federation(error)
    }
}

/// Canonical encoding of one durable history entry as one prefix entry:
/// `domain || task_id(16) || be64(seq) || logical_effect_id(32) ||
/// be64(retry_fence_epoch) || action_proposal_digest(32) ||
/// idempotency_identity_digest(32) || authoritative_effect_receipt_id(16)`.
///
/// The entry's durable identity axes are encoded; the full row content
/// (outcome shape and so on) stays pinned by the store's own recomputed
/// root, which [`converge_commit_prefix_gap`] cross-checks. The
/// `operation_id` placeholder is always `None` in this slice and is
/// therefore not encoded.
fn prefix_entry_bytes(task_id: TaskId, entry: &EffectHistoryEntry) -> Vec<u8> {
    let mut out = Vec::with_capacity(144);
    out.extend_from_slice(PREFIX_ENTRY_DOMAIN);
    out.extend_from_slice(task_id.as_bytes());
    out.extend_from_slice(&entry.effect_history_seq.to_be_bytes());
    out.extend_from_slice(&entry.logical_effect_id);
    out.extend_from_slice(&entry.retry_fence_epoch.to_be_bytes());
    out.extend_from_slice(&entry.action_proposal_digest);
    out.extend_from_slice(&entry.idempotency_identity_digest);
    out.extend_from_slice(entry.authoritative_effect_receipt_id.as_bytes());
    out
}

/// Contiguity check the store's own `[TASK-EFFECT-ID-001]` discipline
/// promises: strictly increasing from 1 with no gaps and no duplicates.
fn history_is_contiguous(task_id: TaskId, entries: &[EffectHistoryEntry]) -> bool {
    entries.iter().enumerate().all(|(index, entry)| {
        entry.task_id == task_id
            && entry.effect_history_seq == u64::try_from(index).expect("index fits u64") + 1
    })
}

/// Shared derivation core: structural checks plus the typed prefix. Every
/// caller-visible fact flows through here so the author side and the
/// convergence side can never disagree about the grammar.
fn derive_durable_prefix(
    task_id: TaskId,
    entries: &[EffectHistoryEntry],
    durable_prefix_root: [u8; 32],
    head_commit_seq: u64,
    head_effect_history_root: [u8; 32],
) -> Result<DurableCommitPrefix, ReconciliationError> {
    if !history_is_contiguous(task_id, entries) {
        return Err(ReconciliationError::InconsistentDurablePrefix {
            reason: "effect history sequence is not contiguous from 1",
        });
    }
    if effect_history_root_of(entries) != durable_prefix_root {
        return Err(ReconciliationError::InconsistentDurablePrefix {
            reason: "store-recomputed root diverges from the durable history formula",
        });
    }
    let encoded = entries
        .iter()
        .map(|entry| prefix_entry_bytes(task_id, entry))
        .collect::<Vec<_>>();
    let entry_count = encoded.len();
    Ok(DurableCommitPrefix {
        prefix: CheckpointPrefix::from_entries(encoded),
        entry_count,
        durable_prefix_root,
        head_commit_seq,
        head_effect_history_root,
    })
}

/// Derives the task authority's durable commit prefix from the store's own
/// read faces: the durable effect history in sequence order, canonically
/// encoded per the module's fixed grammar, the store's recomputed prefix
/// root, and the durable head facts. Checkpoint authors call this same
/// function (then [`CheckpointDigest::from_prefix`] at their own fence
/// axes), so the author and the verifier share one grammar by
/// construction.
///
/// # Errors
///
/// Returns [`ReconciliationError::Store`] when a durable read fails, or
/// [`ReconciliationError::InconsistentDurablePrefix`] when the sequence is
/// not contiguous from 1 or the store's recomputed root diverges from the
/// in-memory formula over the listed entries.
pub fn durable_commit_prefix(
    store: &SqliteTaskAuthority,
    task_id: TaskId,
) -> Result<DurableCommitPrefix, ReconciliationError> {
    let entries = store.list_effect_history(task_id)?;
    let durable_prefix_root = store.compute_effect_history_root(task_id)?;
    let head = store.inspect_task(task_id)?;
    derive_durable_prefix(
        task_id,
        &entries,
        durable_prefix_root,
        head.head_commit_seq,
        head.head_effect_history_root,
    )
}

/// Canonical bytes of the convergence record — the replay-idempotency
/// anchor [`ConvergenceInvariants::replay_digest`] is taken over.
/// Length-prefixed throughout so two distinct records never share a
/// serialization.
fn canonical_record_bytes(outcome: &ReconciliationOutcome, encoded: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(CONVERGENCE_RECORD_DOMAIN);
    out.extend_from_slice(outcome.task_id.as_bytes());
    out.extend_from_slice(outcome.authority_cell.as_bytes());
    out.extend_from_slice(&outcome.term.to_be_bytes());
    out.extend_from_slice(outcome.holder_id.as_bytes());
    out.extend_from_slice(&be64_count(outcome.covered_entries));
    for review in &outcome.trail {
        let record = &review.record;
        let fact = record.fact();
        match review.cover {
            TrailCover::Anchored { covered_entries } => {
                out.push(1);
                out.extend_from_slice(&be64_count(covered_entries));
            }
            TrailCover::FreeText => out.push(2),
        }
        out.extend_from_slice(&record.recorded_ms().to_be_bytes());
        out.extend_from_slice(&u64::from(record.os_process_id()).to_be_bytes());
        out.extend_from_slice(&record.sequence().to_be_bytes());
        out.extend_from_slice(&fact.node_boot_generation().get().to_be_bytes());
        out.extend_from_slice(&fact.epoch().get().to_be_bytes());
        out.extend_from_slice(&fact.fencing_token().get().to_be_bytes());
        let digest = fact.digest().as_bytes();
        out.extend_from_slice(
            &u32::try_from(digest.len())
                .expect("digest axis fits u32")
                .to_be_bytes(),
        );
        out.extend_from_slice(digest);
    }
    out.extend_from_slice(&be64_count(outcome.final_state.entry_count));
    out.extend_from_slice(&outcome.final_state.durable_prefix_root);
    out.extend_from_slice(&outcome.final_state.head_commit_seq.to_be_bytes());
    out.extend_from_slice(&outcome.final_state.head_effect_history_root);
    out.extend_from_slice(&be64_count(outcome.adopted.len()));
    for adopted in &outcome.adopted {
        out.extend_from_slice(&adopted.effect_history_seq.to_be_bytes());
        out.extend_from_slice(&adopted.entry_digest);
    }
    // The final prefix bytes close the canonical form: two records equal
    // in every field above but over different prefixes can never share a
    // replay digest.
    out.extend_from_slice(&be64_count(encoded.len()));
    for entry in encoded {
        out.extend_from_slice(
            &u64::try_from(entry.len())
                .expect("entry fits u64")
                .to_be_bytes(),
        );
        out.extend_from_slice(entry);
    }
    out
}

/// `be64(count)` for usize-typed lengths (they always fit).
fn be64_count(count: usize) -> [u8; 8] {
    u64::try_from(count).expect("usize fits u64").to_be_bytes()
}

/// Classifies one trail record's digest axis against the durable prefix
/// through the CS0-B verifier: the claim anchors at the shortest prefix
/// length whose digest the verifier accepts, free text covers nothing, and
/// a structured body that verifies nowhere fails closed (corrupt body or
/// phantom claim).
fn review_trail_record(
    record: &CheckpointRecord,
    encoded: &[Vec<u8>],
) -> Result<TrailReview, ReconciliationError> {
    let fact = record.fact();
    for covered in 0..=encoded.len() {
        let candidate = CheckpointPrefix::from_entries(&encoded[..covered]);
        if fact.verify_digest(&candidate) == CheckpointDigestVerification::Verified {
            return Ok(TrailReview {
                record: record.clone(),
                cover: TrailCover::Anchored {
                    covered_entries: covered,
                },
            });
        }
    }
    // No prefix length verified — including the full length, so the
    // classification below can never hit `Verified` again.
    let full = CheckpointPrefix::from_entries(encoded);
    match fact.verify_digest(&full) {
        CheckpointDigestVerification::Verified => unreachable!(
            "a digest verified at the full length was already caught by the prefix scan"
        ),
        CheckpointDigestVerification::FreeText => Ok(TrailReview {
            record: record.clone(),
            cover: TrailCover::FreeText,
        }),
        CheckpointDigestVerification::Corrupt => Err(ReconciliationError::TrailDigestCorrupt {
            recorded_ms: record.recorded_ms(),
            os_process_id: record.os_process_id(),
            sequence: record.sequence(),
        }),
        CheckpointDigestVerification::Mismatch {
            presented,
            recomputed,
        } => Err(ReconciliationError::TrailClaimNotDerivable {
            recorded_ms: record.recorded_ms(),
            os_process_id: record.os_process_id(),
            sequence: record.sequence(),
            presented,
            recomputed,
        }),
    }
}

/// Converges the commit-prefix gap: reads the old authority Cell's
/// checkpoint trail, verifies every digest through the CS0-B verifier
/// (fail-closed on mismatch and corruption; free text honestly covers
/// nothing), derives the authority's durable commit prefix, adopts the
/// uncovered gap entries beyond the verified anchor, and returns the typed
/// convergence record with the four ADR-0013 decision 1 contract
/// invariants computed on it.
///
/// The final state derives from the durable prefix alone — the trail only
/// determines where the covered window ends — so different convergence
/// entries (an earlier or later anchor, or no verifiable anchor at all)
/// land on the same unique final state, which is the bounded-convergence
/// shape ADR-0019 R1.2.4 fixes. The executor draws no clock and mints no
/// identity: an identical rerun produces a byte-identical record.
///
/// # Errors
///
/// Returns [`ReconciliationError::NotTermHolder`] /
/// [`ReconciliationError::NoActiveAssignment`] when the store's active
/// assignment is not bound to the presented lease (the dual-primary
/// guard, checked before any trail read), the fail-closed trail errors
/// ([`ReconciliationError::TrailFilesCorrupt`],
/// [`ReconciliationError::TrailDigestCorrupt`],
/// [`ReconciliationError::TrailClaimNotDerivable`]), the durable-prefix
/// structural error ([`ReconciliationError::InconsistentDurablePrefix`]),
/// or a carried store/federation failure.
///
/// # Panics
///
/// Panics only on arithmetic that cannot fail for real prefix sizes
/// (usize-to-u64 conversions) and if the anchored trail record the
/// outcome itself just built cannot be found again in the invariant
/// re-verification — both are structural impossibilities the type system
/// cannot state, not runtime conditions to handle.
#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)] // One function keeps the convergence leg order visible.
pub fn converge_commit_prefix_gap(
    request: ReconciliationRequest<'_>,
) -> Result<ReconciliationOutcome, ReconciliationError> {
    // Leg 0 — term precondition: the dual-primary guard, before any trail
    // or prefix fact is read.
    let active = match request.store.inspect_authority_assignment(request.task_id) {
        Ok(record) => record,
        Err(TaskStoreError::ReceiptNotFound) => {
            return Err(ReconciliationError::NoActiveAssignment);
        }
        Err(error) => return Err(error.into()),
    };
    if active.state != AuthorityAssignmentState::Active
        || active.authority_lease_binding != request.term_lease.binding()
    {
        return Err(ReconciliationError::NotTermHolder {
            presented_term: request.term_lease.term,
            active_term: active.authority_lease_binding.term,
        });
    }

    // Leg 1 — the durable prefix: the convergence's only source of
    // final-state truth, through the store's own read faces.
    let entries = request.store.list_effect_history(request.task_id)?;
    let durable_prefix_root = request.store.compute_effect_history_root(request.task_id)?;
    let head = request.store.inspect_task(request.task_id)?;
    let durable = derive_durable_prefix(
        request.task_id,
        &entries,
        durable_prefix_root,
        head.head_commit_seq,
        head.head_effect_history_root,
    )?;
    let encoded: &[Vec<u8>] = durable.prefix().entries();

    // Leg 2 — the trail: enumerated for exactly the old authority Cell,
    // fail-closed on corrupt files, every digest verified through the
    // CS0-B face.
    let log = request.directory.checkpoints(request.authority_cell)?;
    if log.corrupt() > 0 {
        return Err(ReconciliationError::TrailFilesCorrupt {
            count: log.corrupt(),
        });
    }
    let mut trail = Vec::new();
    for record in log.into_records() {
        trail.push(review_trail_record(&record, encoded)?);
    }

    // Leg 3 — the anchor: the longest verified cover, earliest record on
    // ties (a pure function of the trail).
    let mut covered_entries = 0;
    let mut anchor: Option<VerifiedAnchor> = None;
    for review in &trail {
        if let TrailCover::Anchored {
            covered_entries: at,
        } = review.cover
            && at > covered_entries
        {
            covered_entries = at;
            let fact = review.record.fact();
            let pinned = CheckpointPrefix::from_entries(&encoded[..at]);
            anchor = Some(VerifiedAnchor {
                recorded_ms: review.record.recorded_ms(),
                os_process_id: review.record.os_process_id(),
                sequence: review.record.sequence(),
                covered_entries: at,
                digest: CheckpointDigest::from_prefix(
                    fact.node_boot_generation(),
                    fact.epoch(),
                    fact.fencing_token(),
                    &pinned,
                ),
            });
        }
    }

    // Leg 4 — gap adoption: durable entries beyond the anchor. Adoption is
    // bookkeeping over already-durable rows, never a re-execution.
    let adopted = entries[covered_entries..]
        .iter()
        .map(|entry| {
            let mut hasher = Sha256::new();
            hasher.update(GAP_ENTRY_DOMAIN);
            hasher.update(prefix_entry_bytes(request.task_id, entry));
            AdoptedEntry {
                effect_history_seq: entry.effect_history_seq,
                entry_digest: hasher.finalize().into(),
            }
        })
        .collect::<Vec<_>>();

    let final_state = FinalAuthorityState {
        entry_count: durable.entry_count(),
        durable_prefix_root: durable.durable_prefix_root(),
        head_commit_seq: durable.head_commit_seq(),
        head_effect_history_root: durable.head_effect_history_root(),
    };
    let visibility = if anchor.is_none() {
        PrefixVisibility::Uncertain
    } else if covered_entries < final_state.entry_count {
        PrefixVisibility::Partial
    } else {
        PrefixVisibility::Covered
    };
    let outcome = ReconciliationOutcome {
        authority_cell: request.authority_cell,
        task_id: request.task_id,
        term: request.term_lease.term,
        holder_id: request.term_lease.holder_id,
        anchor,
        trail,
        visibility,
        covered_entries,
        adopted,
        final_state,
        invariants: ConvergenceInvariants {
            crash_window_converged: false,
            no_phantom_rows: false,
            no_double_commit: false,
            replay_digest: [0; 32],
        },
    };

    // Leg 5 — the four contract invariants, each computed on the assembled
    // facts and fail-closed when violated.
    //
    // Invariant 1 — crash-window convergence: the anchor's digest
    // re-verifies over exactly the covered prefix bytes at its own fence
    // axes (the same re-derivation discipline as the CS0-A readbacks),
    // and covered plus adopted compose exactly the final length.
    let anchor_reverifies = match &outcome.anchor {
        None => covered_entries == 0,
        Some(anchor) => {
            // The axes live on the anchored trail record; re-read them from
            // the review so the check cannot drift from the anchor's own
            // data path.
            let review = outcome
                .trail
                .iter()
                .find(|review| {
                    review.record.recorded_ms() == anchor.recorded_ms
                        && review.record.os_process_id() == anchor.os_process_id
                        && review.record.sequence() == anchor.sequence
                })
                .expect("the anchor names a record the trail carries");
            let fact = review.record.fact();
            CheckpointDigest::from_prefix(
                fact.node_boot_generation(),
                fact.epoch(),
                fact.fencing_token(),
                &CheckpointPrefix::from_entries(&encoded[..covered_entries]),
            ) == anchor.digest
        }
    };
    let composes_exactly =
        covered_entries + outcome.adopted.len() == outcome.final_state.entry_count;

    // Invariant 2 — no phantom rows: the final prefix is exactly the
    // store's durable history, contiguous from 1, with the store's
    // recomputed root equal to the in-memory formula over the listed
    // entries (re-derived here as computed facts, not assumed).
    let no_phantom_rows = history_is_contiguous(request.task_id, &entries)
        && effect_history_root_of(&entries) == outcome.final_state.durable_prefix_root;

    // Invariant 3 — no double commit: no duplicate sequences (contiguity
    // implies it over the whole prefix) and every adopted entry strictly
    // beyond the anchor boundary and strictly increasing among
    // themselves — trail-covered entries are never re-adopted.
    let boundary_seq = entries
        .get(covered_entries.wrapping_sub(1))
        .map_or(0, |entry| entry.effect_history_seq);
    let adopted_strictly_beyond = outcome
        .adopted
        .iter()
        .all(|entry| entry.effect_history_seq > boundary_seq);
    let adopted_strictly_increasing = outcome
        .adopted
        .windows(2)
        .all(|pair| pair[0].effect_history_seq < pair[1].effect_history_seq);

    // Invariant 4 — the replay digest over the canonical record bytes.
    let mut hasher = Sha256::new();
    hasher.update(canonical_record_bytes(&outcome, encoded));

    let checked = ConvergenceInvariants {
        crash_window_converged: anchor_reverifies && composes_exactly,
        no_phantom_rows,
        no_double_commit: adopted_strictly_beyond && adopted_strictly_increasing,
        replay_digest: hasher.finalize().into(),
    };
    if !checked.crash_window_converged {
        return Err(ReconciliationError::InconsistentDurablePrefix {
            reason: "crash-window composition check failed",
        });
    }
    if !checked.no_phantom_rows {
        return Err(ReconciliationError::InconsistentDurablePrefix {
            reason: "phantom-row check failed",
        });
    }
    if !checked.no_double_commit {
        return Err(ReconciliationError::InconsistentDurablePrefix {
            reason: "double-commit check failed",
        });
    }

    Ok(ReconciliationOutcome {
        invariants: checked,
        ..outcome
    })
}
