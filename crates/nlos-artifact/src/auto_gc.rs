//! Caller-driven automatic orphan-GC trigger (W28-E): a periodic or
//! threshold condition evaluated on an explicit tick.
//!
//! The deletion core is untouched: every triggered pass is exactly one
//! [`ArtifactStore::collect_orphan_blobs`] call (`gc` module — same
//! three-layer conservative reference set, same delete-then-receipt
//! order). This module only decides *whether* one pass runs.
//!
//! # Shape (and what it deliberately is not)
//!
//! - **No background thread, no open-time sweep.** The caller owns the
//!   clock and the cadence: it invokes [`ArtifactStore::tick_auto_gc`]
//!   whenever it wants (after open, from its own timer loop, before
//!   install flows). Open latency stays predictable; a library-shaped,
//!   caller-driven, fail-closed trigger.
//! - **`Disabled` is a full bypass** (the `AutoOrphanGc` precedent): no
//!   scan, no pass, no key consumed, no counter touched.
//! - An `Enabled` policy with neither a period nor a threshold can never
//!   fire and reports [`AutoGcSkipReason::NeverEligible`] instead of
//!   guessing.
//!
//! # Trigger conditions (either one fires one pass)
//!
//! - **Period**: `now_ms >= last_pass_at_ms + interval_ms` (a store that
//!   never ran a pass is immediately eligible).
//! - **Threshold**: when the period has not elapsed, an optional
//!   `orphan_threshold` counts the current orphan candidates (the same
//!   read-only present-minus-referenced diff the core would compute) and
//!   fires when `candidates >= threshold`. The count only *decides*; the
//!   pass that follows recomputes everything inside its own transaction,
//!   so a racy count can cause at most one extra conservative pass —
//!   never a wrong deletion.
//!
//! # Independent idempotency-key space
//!
//! Triggered passes never accept caller keys. Pass number `k` (the
//! durable `passes_completed` counter, zero-based) runs under the
//! domain-separated derived key
//! `SHA-256("llmos/artifact-auto-gc-key/v1" || k_be)[0..16]` — a 128-bit
//! space disjoint by construction from the manual receipt keys and from
//! every other caller-supplied or runtime-derived key family (the
//! slice-key manual/install offsets 19/20 and 21/22 included).
//!
//! # Pass protocol and crash windows
//!
//! 1. Read the durable state; if eligible, let `k = passes_completed`
//!    and derive the pass key. (The index is *not* reserved first: the
//!    single-writer mutex plus the compare-and-set below make the
//!    advance exactly-once even under interleaved ticks.)
//! 2. Run [`ArtifactStore::collect_orphan_blobs`] with that key. A
//!    receipt left by an earlier attempt of the same `k` replays
//!    verbatim (the crate's durable-authority replay rule).
//! 3. Advance the state in one transaction with a compare-and-set on
//!    `passes_completed = k`: `passes_completed + 1`,
//!    `orphans_collected_total + receipt.collected_count`,
//!    `last_pass_at_ms = now_ms`. A tick that observes the CAS already
//!    consumed reports its receipt without double counting.
//!
//! A crash between steps 2 and 3 leaves the receipt durable and the
//! state stale; the next eligible tick re-derives the same key, replays
//! the receipt, and advances exactly once. A crash inside step 2 leaves
//! the documented `gc` pre-receipt window (consistent, idempotent retry
//! under the *same* key — the failed pass index is not consumed).
//!
//! A failed pass increments `failure_count` and `last_failure_at_ms`,
//! grants no pass credit, and does **not** reset the periodic window, so
//! the next tick re-attempts pass `k` under the same key. Trigger
//! evaluation errors (state read, threshold count) do not count as pass
//! failures. `orphans_collected_total` counts only receipt-recorded
//! collections: removals by an attempt that never committed its receipt
//! are attributed to the completing retry.
//!
//! The mutable state row (`artifact_auto_gc_state`) is deliberately not
//! immutable-trigger-protected: it is an aggregate health counter, not
//! an authority record — the per-pass receipts stay immutable.

use nlos_types::IdempotencyKey;
use rusqlite::{TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::ArtifactError;
use crate::gc::{CollectOrphanBlobsDecision, CollectOrphanBlobsRequest, GcReceipt};
use crate::store::{ArtifactStore, encode_u64};

/// Whether and how one [`ArtifactStore::tick_auto_gc`] may run an
/// automatic orphan-GC pass (the `AutoOrphanGc` bypass precedent).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutoOrphanGcPolicy {
    /// Full opt-out: no scan, no pass, no key consumed, no counter
    /// touched.
    Disabled,
    /// Tick trigger conditions; either one being met fires one pass.
    Enabled {
        /// Periodic condition: fire when `now_ms` is at least
        /// `interval_ms` past the last completed pass. `None` disables
        /// the periodic condition. `Some(0)` fires on every tick.
        interval_ms: Option<u64>,
        /// Threshold condition, probed only while the period has not
        /// elapsed: fire when the current orphan-candidate count is at
        /// least this. `None` disables the threshold condition. Must be
        /// at least 1 when present (`Some(0)` is rejected as the
        /// degenerate "always fire after a scan").
        orphan_threshold: Option<u64>,
    },
}

/// Request for one [`ArtifactStore::tick_auto_gc`] evaluation.
#[derive(Clone, Copy, Debug)]
pub struct TickAutoGcRequest {
    /// Caller-supplied evaluation time (milliseconds since Unix epoch),
    /// crate-wide time-source discipline: it is also the
    /// `collected_at_ms` of any pass this tick fires.
    pub now_ms: u64,
    /// The trigger policy for this tick.
    pub policy: AutoOrphanGcPolicy,
}

/// Why a tick did not run a pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutoGcSkipReason {
    /// [`AutoOrphanGcPolicy::Disabled`]: nothing was evaluated,
    /// scanned, or consumed.
    Disabled,
    /// The policy enables no trigger condition; a pass can never fire.
    NeverEligible,
    /// The periodic condition has not elapsed yet.
    IntervalNotElapsed {
        /// Completion time of the last pass the window counts from.
        last_pass_at_ms: u64,
        /// The earliest `now_ms` the next periodic pass may fire at.
        eligible_at_ms: u64,
    },
    /// The threshold condition was probed and not met.
    BelowThreshold {
        /// Orphan candidates counted at evaluation time.
        orphan_candidates: u64,
        /// The configured threshold.
        orphan_threshold: u64,
    },
}

/// Outcome of [`ArtifactStore::tick_auto_gc`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AutoGcTickDecision {
    /// The trigger fired and a fresh pass ran to completion.
    Collected(GcReceipt),
    /// The trigger fired and replayed the durable receipt of an earlier
    /// attempt of the same pass index (crash/interleave convergence).
    Replayed(GcReceipt),
    /// No pass ran; the reason says which condition gated it.
    Skipped(AutoGcSkipReason),
}

impl AutoGcTickDecision {
    /// The pass receipt, when one ran or replayed.
    #[must_use]
    pub const fn receipt(&self) -> Option<&GcReceipt> {
        match self {
            Self::Collected(receipt) | Self::Replayed(receipt) => Some(receipt),
            Self::Skipped(_) => None,
        }
    }
}

/// Typed durable readback of the automatic GC trigger's health
/// (the `artifact_auto_gc_state` singleton row).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AutoGcHealth {
    /// Completed passes; also the index of the next pass key.
    pub passes_completed: u64,
    /// Sum of `collected_count` over completed passes' receipts.
    pub orphans_collected_total: u64,
    /// Failed pass attempts (never reset by later successes).
    pub failure_count: u64,
    /// Completion time of the last pass (`None` = never ran).
    pub last_pass_at_ms: Option<u64>,
    /// Evaluation time of the last failed pass (`None` = none failed).
    pub last_failure_at_ms: Option<u64>,
}

impl ArtifactStore {
    /// Evaluates the automatic orphan-GC trigger once and, when a
    /// condition is met, runs one conservative orphan-collection pass
    /// over the unchanged [`ArtifactStore::collect_orphan_blobs`] core.
    ///
    /// A fired pass is serialized against `put_revision`/`stage_revision`
    /// by the shared writer mutex (see the `gc` module): the blob-commit
    /// phases of those writes run inside the same critical section as
    /// the pass's scan, so no in-flight blob is ever sentenced.
    ///
    /// # Errors
    ///
    /// Returns a trigger-evaluation error (state read, threshold count)
    /// without counting a failure, or the fired pass's error after
    /// durably recording the failure. If the failure-recording write
    /// itself fails, that storage error is returned (the pass error's
    /// remediation — retry the tick — is unchanged).
    pub fn tick_auto_gc(
        &self,
        request: TickAutoGcRequest,
    ) -> Result<AutoGcTickDecision, ArtifactError> {
        let AutoOrphanGcPolicy::Enabled {
            interval_ms,
            orphan_threshold,
        } = request.policy
        else {
            return Ok(AutoGcTickDecision::Skipped(AutoGcSkipReason::Disabled));
        };
        if orphan_threshold == Some(0) {
            return Err(ArtifactError::InvalidSpec(
                "auto gc orphan threshold must be at least one",
            ));
        }
        if interval_ms.is_none() && orphan_threshold.is_none() {
            return Ok(AutoGcTickDecision::Skipped(AutoGcSkipReason::NeverEligible));
        }

        let state = self.load_auto_gc_state()?;
        // Periodic condition: a Some interval fires once the last
        // completed pass is `interval` old; a store that never ran a
        // pass is immediately eligible.
        let periodic_due = match (interval_ms, state.last_pass_at_ms) {
            (Some(_), None) => true,
            (Some(interval), Some(last_pass_at_ms)) => {
                request.now_ms >= last_pass_at_ms.saturating_add(interval)
            }
            (None, _) => false,
        };

        if !periodic_due {
            if let Some(orphan_threshold) = orphan_threshold {
                let orphan_candidates = self.count_orphan_candidates()?;
                if orphan_candidates < orphan_threshold {
                    return Ok(AutoGcTickDecision::Skipped(
                        AutoGcSkipReason::BelowThreshold {
                            orphan_candidates,
                            orphan_threshold,
                        },
                    ));
                }
                // Threshold met: fall through and fire the pass.
            } else {
                // The only non-due shape without a threshold is a Some
                // interval with a recorded last pass; the unreachable
                // arm stays fail-closed rather than guessing.
                return match (interval_ms, state.last_pass_at_ms) {
                    (Some(interval), Some(last_pass_at_ms)) => Ok(AutoGcTickDecision::Skipped(
                        AutoGcSkipReason::IntervalNotElapsed {
                            last_pass_at_ms,
                            eligible_at_ms: last_pass_at_ms.saturating_add(interval),
                        },
                    )),
                    _ => Ok(AutoGcTickDecision::Skipped(AutoGcSkipReason::NeverEligible)),
                };
            }
        }

        let pass_index = state.passes_completed;
        let pass_key = auto_gc_pass_key(pass_index);
        let pass = self.collect_orphan_blobs(CollectOrphanBlobsRequest {
            idempotency_key: pass_key,
            collected_at_ms: request.now_ms,
        });
        let decision = match pass {
            Ok(CollectOrphanBlobsDecision::Collected(receipt)) => {
                self.advance_auto_gc_state(pass_index, &receipt, request.now_ms)?;
                AutoGcTickDecision::Collected(receipt)
            }
            Ok(CollectOrphanBlobsDecision::Replayed(receipt)) => {
                self.advance_auto_gc_state(pass_index, &receipt, request.now_ms)?;
                AutoGcTickDecision::Replayed(receipt)
            }
            Err(error) => {
                self.record_auto_gc_failure(request.now_ms)?;
                return Err(error);
            }
        };
        Ok(decision)
    }

    /// Reads the durable automatic-GC health counters.
    ///
    /// # Errors
    ///
    /// Returns a storage or corruption error.
    pub fn inspect_auto_gc_health(&self) -> Result<AutoGcHealth, ArtifactError> {
        self.load_auto_gc_state()
    }

    fn load_auto_gc_state(&self) -> Result<AutoGcHealth, ArtifactError> {
        let connection = self.lock_connection()?;
        let result = connection.query_row(
            "SELECT passes_completed, orphans_collected_total, failure_count,
                    last_pass_at_ms, last_failure_at_ms
             FROM artifact_auto_gc_state WHERE singleton = 0",
            [],
            decode_auto_gc_health,
        );
        match result {
            Ok(health) => Ok(health),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                Err(ArtifactError::CorruptRecord("auto gc state row missing"))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Compare-and-set advance on `passes_completed = pass_index`, so a
    /// stale concurrent tick cannot double count. Zero changed rows
    /// means another tick already advanced this index.
    fn advance_auto_gc_state(
        &self,
        pass_index: u64,
        receipt: &GcReceipt,
        now_ms: u64,
    ) -> Result<(), ArtifactError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE artifact_auto_gc_state
             SET passes_completed = passes_completed + 1,
                 orphans_collected_total = orphans_collected_total + ?2,
                 last_pass_at_ms = ?3
             WHERE singleton = 0 AND passes_completed = ?1",
            params![
                encode_u64(pass_index)?,
                encode_u64(receipt.collected_count)?,
                encode_u64(now_ms)?,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn record_auto_gc_failure(&self, now_ms: u64) -> Result<(), ArtifactError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE artifact_auto_gc_state
             SET failure_count = failure_count + 1, last_failure_at_ms = ?1
             WHERE singleton = 0",
            params![encode_u64(now_ms)?],
        )?;
        transaction.commit()?;
        Ok(())
    }
}

/// Domain-separated derived key of pass `pass_index`: the trigger's own
/// 128-bit key space, disjoint by construction from every caller key
/// family (the slice-key manual 19/20 and install 21/22 offsets
/// included).
fn auto_gc_pass_key(pass_index: u64) -> IdempotencyKey {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/artifact-auto-gc-key/v1");
    hasher.update(pass_index.to_be_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    IdempotencyKey::from_bytes(bytes)
}

/// Decodes the STRICT singleton row; the CHECK constraints make the
/// `unwrap_or` arms unreachable for well-formed storage (gc.rs decode
/// precedent).
fn decode_auto_gc_health(row: &rusqlite::Row<'_>) -> rusqlite::Result<AutoGcHealth> {
    Ok(AutoGcHealth {
        passes_completed: u64::try_from(row.get::<_, i64>(0)?).unwrap_or(u64::MAX),
        orphans_collected_total: u64::try_from(row.get::<_, i64>(1)?).unwrap_or(u64::MAX),
        failure_count: u64::try_from(row.get::<_, i64>(2)?).unwrap_or(u64::MAX),
        last_pass_at_ms: row
            .get::<_, Option<i64>>(3)?
            .and_then(|value| u64::try_from(value).ok()),
        last_failure_at_ms: row
            .get::<_, Option<i64>>(4)?
            .and_then(|value| u64::try_from(value).ok()),
    })
}
