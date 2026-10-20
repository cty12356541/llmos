//! Cell-local failure detector — first piece of the §26.1 seven-piece set.
//!
//! Scope (v0.5 §26.1 + `[DIST-FAIL-001]`): one Cell watches the liveness of
//! locally tracked subjects through caller-reported evidence. The detector
//! owns no transport, reads no wall clock, and sends no signals: it converts
//! a monotone caller-supplied logical tick plus heartbeat arrivals into
//! typed, epoch-scoped liveness verdicts. Position transparency never hides
//! failure semantics here — every verdict is derived from explicit evidence
//! (`Alive` only from a heartbeat, `Suspect`/`Dead` only from measured
//! silence).
//!
//! Escalation is monotone per tracked incarnation: `Alive → Suspect → Dead`
//! advances as silence grows against the configured thresholds and never
//! regresses while the subject stays silent; a heartbeat is the only way
//! back to `Alive`, and it clears an unproven suspicion but never revives a
//! `Dead` incarnation — judgments are history, not state.
//!
//! Epoch-loss linkage (the explicit §12/§26.1 fence contract of this
//! piece): when the Cell authority advances its epoch, the detector closes
//! every pending suspicion as **void** — an unproven suspicion never
//! promotes to `Dead` across an epoch boundary, and heartbeat evidence from
//! the old epoch never counts as liveness evidence in the new one (each
//! subject must present a fresh heartbeat). `Dead` judgments recorded under
//! an old epoch stay immutable history; their subjects re-enter tracking as
//! a fresh incarnation in the new epoch, so verdicts stay fenced by
//! `(identity, epoch, incarnation)` and an old instance cannot silently
//! revive (`[DIST-NAME-001]`).
//!
//! This module is pure in-memory single-Cell state: no threads, no
//! persistence, no IPC. Durability, when the seven-piece assembly wires it,
//! belongs to the Cell's durable event/outbox face, not here.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;

use nlos_types::{ExecutionFiberId, ProcessId};

use crate::{CellEpoch, CellFence, CellIdentity};

/// One watched subject. The first slice supports the fiber and process
/// abstraction ids; the enum keeps them type-distinct so a fiber id can
/// never be confused with a process id.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum MonitoredSubject {
    /// An execution fiber inside some process.
    Fiber(ExecutionFiberId),
    /// A delegated process (`ProcessId` is stable; incarnation is tracked
    /// separately by the detector, not encoded in the id).
    Process(ProcessId),
}

/// Liveness verdict for one subject in one epoch and incarnation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Liveness {
    /// Heartbeat evidence within the configured `suspect_after` window.
    Alive,
    /// Silent for at least `suspect_after` ticks but fewer than
    /// `dead_after`; unproven, never promoted across an epoch boundary.
    Suspect,
    /// Silent for at least `dead_after` ticks; a closed judgment for this
    /// incarnation (immutable history).
    Dead,
}

/// Escalation thresholds in caller logical ticks.
///
/// `dead_after` must be greater than or equal to `suspect_after`; a subject
/// silent for exactly `suspect_after` ticks is `Suspect`, and one silent for
/// exactly `dead_after` ticks is `Dead`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FailureDetectorConfig {
    suspect_after: NonZeroU64,
    dead_after: NonZeroU64,
}

impl FailureDetectorConfig {
    /// Builds a config, failing closed when the `Dead` threshold precedes
    /// the `Suspect` threshold (that ordering would make `Suspect`
    /// unreachable).
    ///
    /// # Errors
    ///
    /// Returns [`FailureDetectorError::DeadThresholdBeforeSuspect`] when
    /// `dead_after < suspect_after`.
    pub fn new(
        suspect_after: NonZeroU64,
        dead_after: NonZeroU64,
    ) -> Result<Self, FailureDetectorError> {
        if dead_after < suspect_after {
            return Err(FailureDetectorError::DeadThresholdBeforeSuspect {
                suspect_after,
                dead_after,
            });
        }
        Ok(Self {
            suspect_after,
            dead_after,
        })
    }

    /// Silence window (in ticks) after which a subject becomes `Suspect`.
    #[must_use]
    pub const fn suspect_after(self) -> NonZeroU64 {
        self.suspect_after
    }

    /// Silence window (in ticks) after which a subject becomes `Dead`.
    #[must_use]
    pub const fn dead_after(self) -> NonZeroU64 {
        self.dead_after
    }
}

/// One closed `Dead` judgment. Append-only history: it is never mutated or
/// revoked, and it stays fenced by the epoch it formed in.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeadJudgment {
    /// Subject the judgment closed on.
    pub subject: MonitoredSubject,
    /// Epoch the judgment formed in.
    pub epoch: CellEpoch,
    /// Logical tick the silence crossed `dead_after`.
    pub dead_at: u64,
    /// Incarnation of the subject inside `epoch` (1-based).
    pub incarnation: u64,
}

/// Observation of one tracked subject.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LivenessView {
    /// Subject this view describes.
    pub subject: MonitoredSubject,
    /// Current verdict.
    pub liveness: Liveness,
    /// Epoch the current tracking incarnation opened in.
    pub epoch: CellEpoch,
    /// Incarnation counter: prior `Dead` judgments for this subject plus
    /// one. Monotone across epochs; never decreases.
    pub incarnation: u64,
    /// Last heartbeat tick inside this epoch, if any evidence arrived yet.
    pub last_heartbeat: Option<u64>,
    /// First tick this incarnation reached `Suspect` and stayed silent;
    /// `None` while `Alive` or already `Dead`.
    pub suspected_at: Option<u64>,
    /// Tick this incarnation was judged `Dead`; `None` while not `Dead`.
    pub dead_at: Option<u64>,
}

/// Outcome of an accepted heartbeat.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HeartbeatOutcome {
    /// Whether the heartbeat cleared an unproven `Suspect` verdict.
    pub cleared_suspicion: bool,
}

/// Result of one escalation sweep.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SweepReport {
    /// Epoch the sweep ran in.
    pub epoch: CellEpoch,
    /// Subjects newly escalated `Alive → Suspect`, with the suspicion tick.
    /// Sorted by subject.
    pub newly_suspected: Vec<(MonitoredSubject, u64)>,
    /// Judgments newly closed `Suspect/Alive → Dead`. Sorted by subject.
    pub newly_dead: Vec<DeadJudgment>,
}

/// Result of closing the epoch: what the detector voided on the fence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EpochAdvanceReport {
    /// Epoch the detector ran in before the advance.
    pub previous_epoch: CellEpoch,
    /// Epoch the detector now runs in.
    pub new_epoch: CellEpoch,
    /// Pending suspicions closed as void, with the tick each suspicion had
    /// formed at. None of these promote to `Dead` across the boundary.
    /// Sorted by subject.
    pub suspicions_voided: Vec<(MonitoredSubject, u64)>,
    /// Subjects whose prior incarnation was already `Dead` and that re-enter
    /// tracking under a fresh incarnation in `new_epoch`. Sorted by subject.
    pub re_tracked_after_death: Vec<MonitoredSubject>,
}

/// Errors from configuring or driving the failure detector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureDetectorError {
    /// Caller presented a logical tick older than one the detector already
    /// observed; the caller clock must be monotone.
    ClockRegression {
        /// Tick the caller presented.
        presented: u64,
        /// Newest tick the detector has already observed.
        last_seen: u64,
    },
    /// A heartbeat arrived for a subject whose current incarnation is
    /// already judged `Dead`; a dead incarnation cannot revive in place.
    SubjectAlreadyDead {
        /// Subject the heartbeat was reported for.
        subject: MonitoredSubject,
        /// Epoch the dead judgment formed in.
        epoch: CellEpoch,
        /// Incarnation that is dead.
        incarnation: u64,
    },
    /// The presented fence does not strictly advance the epoch (equal or
    /// older); epoch fences only move forward.
    EpochNotAdvanced {
        /// Epoch on the presented fence.
        presented: CellEpoch,
        /// Epoch the detector currently runs in.
        current: CellEpoch,
    },
    /// The presented fence belongs to a different Cell identity.
    IdentityMismatch {
        /// Identity on the presented fence.
        presented: CellIdentity,
        /// Identity this detector is bound to.
        current: CellIdentity,
    },
    /// Config would order the `Dead` threshold before the `Suspect`
    /// threshold.
    DeadThresholdBeforeSuspect {
        /// Configured `Suspect` threshold.
        suspect_after: NonZeroU64,
        /// Configured `Dead` threshold.
        dead_after: NonZeroU64,
    },
}

impl fmt::Display for FailureDetectorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClockRegression {
                presented,
                last_seen,
            } => write!(
                formatter,
                "failure-detector logical clock regression: presented {presented} < last seen {last_seen}"
            ),
            Self::SubjectAlreadyDead {
                subject,
                epoch,
                incarnation,
            } => write!(
                formatter,
                "heartbeat for {subject:?} rejected: incarnation {incarnation} already dead in epoch {}",
                epoch.get()
            ),
            Self::EpochNotAdvanced { presented, current } => write!(
                formatter,
                "failure-detector epoch must strictly advance: presented {} <= current {}",
                presented.get(),
                current.get()
            ),
            Self::IdentityMismatch { presented, current } => write!(
                formatter,
                "failure-detector identity mismatch: presented {presented:?} != current {current:?}"
            ),
            Self::DeadThresholdBeforeSuspect {
                suspect_after,
                dead_after,
            } => write!(
                formatter,
                "dead threshold {} precedes suspect threshold {}",
                dead_after.get(),
                suspect_after.get()
            ),
        }
    }
}

impl Error for FailureDetectorError {}

/// Internal per-subject tracking record (one incarnation).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SubjectRecord {
    /// Epoch this incarnation opened in.
    epoch: CellEpoch,
    /// Incarnation of the subject (1-based). Bumps only when a `Dead`
    /// record re-enters tracking through epoch advance; saturates at the
    /// counter top rather than wrapping (judgments stay distinct by epoch
    /// and tick regardless).
    incarnation: NonZeroU64,
    /// Tick baseline used to measure silence before the first heartbeat of
    /// this incarnation (record creation or the epoch-advance tick).
    baseline_tick: u64,
    /// Last accepted heartbeat tick in this incarnation.
    last_heartbeat: Option<u64>,
    /// First tick this incarnation was escalated to `Suspect`.
    suspected_at: Option<u64>,
    /// Tick this incarnation was judged `Dead`.
    dead_at: Option<u64>,
}

impl SubjectRecord {
    fn liveness(self) -> Liveness {
        if self.dead_at.is_some() {
            Liveness::Dead
        } else if self.suspected_at.is_some() {
            Liveness::Suspect
        } else {
            Liveness::Alive
        }
    }

    fn silence_since(self) -> u64 {
        self.last_heartbeat.unwrap_or(self.baseline_tick)
    }
}

/// Cell-local failure detector bound to one Cell identity and epoch.
///
/// Construct it from the authority's current fence snapshot; it never owns
/// or claims the authority itself, so it composes with the other Cell-local
/// pieces without contending the process-scoped claim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailureDetector {
    identity: CellIdentity,
    epoch: CellEpoch,
    config: FailureDetectorConfig,
    last_tick: u64,
    subjects: HashMap<MonitoredSubject, SubjectRecord>,
    dead_judgments: Vec<DeadJudgment>,
}

impl FailureDetector {
    /// Binds a detector to the Cell fence snapshot and escalation config.
    #[must_use]
    pub fn new(fence: &CellFence, config: FailureDetectorConfig) -> Self {
        Self {
            identity: fence.identity(),
            epoch: fence.epoch(),
            config,
            last_tick: 0,
            subjects: HashMap::new(),
            dead_judgments: Vec::new(),
        }
    }

    /// Identity this detector is bound to.
    #[must_use]
    pub const fn identity(&self) -> CellIdentity {
        self.identity
    }

    /// Epoch this detector currently runs in.
    #[must_use]
    pub const fn epoch(&self) -> CellEpoch {
        self.epoch
    }

    /// Escalation thresholds in use.
    #[must_use]
    pub const fn config(&self) -> FailureDetectorConfig {
        self.config
    }

    /// Number of currently tracked subjects (any verdict).
    #[must_use]
    pub fn subject_count(&self) -> usize {
        self.subjects.len()
    }

    /// Append-only history of closed `Dead` judgments, oldest first.
    #[must_use]
    pub fn dead_judgments(&self) -> &[DeadJudgment] {
        &self.dead_judgments
    }

    /// Observes one tracked subject, or `None` if it has no record (no
    /// heartbeat ever accepted).
    #[must_use]
    pub fn liveness(&self, subject: MonitoredSubject) -> Option<LivenessView> {
        let record = self.subjects.get(&subject)?;
        Some(LivenessView {
            subject,
            liveness: record.liveness(),
            epoch: record.epoch,
            incarnation: record.incarnation.get(),
            last_heartbeat: record.last_heartbeat,
            suspected_at: record.suspected_at,
            dead_at: record.dead_at,
        })
    }

    /// Reports liveness evidence for `subject` at logical tick `now`.
    ///
    /// Creates the tracking record on first evidence, clears any unproven
    /// `Suspect` verdict, and never revives a `Dead` incarnation (that is
    /// a typed reject; the subject re-enters tracking only through epoch
    /// advance).
    ///
    /// # Errors
    ///
    /// Returns [`FailureDetectorError::ClockRegression`] when `now` is
    /// older than an already-observed tick, and
    /// [`FailureDetectorError::SubjectAlreadyDead`] when the subject's
    /// current incarnation is already judged `Dead`.
    pub fn record_heartbeat(
        &mut self,
        subject: MonitoredSubject,
        now: u64,
    ) -> Result<HeartbeatOutcome, FailureDetectorError> {
        self.observe_tick(now)?;
        let cleared_suspicion;
        if let Some(record) = self.subjects.get_mut(&subject) {
            if record.dead_at.is_some() {
                return Err(FailureDetectorError::SubjectAlreadyDead {
                    subject,
                    epoch: record.epoch,
                    incarnation: record.incarnation.get(),
                });
            }
            cleared_suspicion = record.suspected_at.is_some();
            record.epoch = self.epoch;
            record.baseline_tick = now;
            record.last_heartbeat = Some(now);
            record.suspected_at = None;
        } else {
            cleared_suspicion = false;
            self.subjects.insert(
                subject,
                SubjectRecord {
                    epoch: self.epoch,
                    incarnation: NonZeroU64::MIN,
                    baseline_tick: now,
                    last_heartbeat: Some(now),
                    suspected_at: None,
                    dead_at: None,
                },
            );
        }
        Ok(HeartbeatOutcome { cleared_suspicion })
    }

    /// Escalates every non-`Dead` subject against the silence thresholds
    /// and closes new `Dead` judgments.
    ///
    /// Idempotent at a fixed tick: sweeping twice at the same `now` reports
    /// nothing new. Suspicion keeps its first tick while the subject stays
    /// silent (monotone escalation).
    ///
    /// # Errors
    ///
    /// Returns [`FailureDetectorError::ClockRegression`] when `now` is
    /// older than an already-observed tick.
    pub fn sweep(&mut self, now: u64) -> Result<SweepReport, FailureDetectorError> {
        self.observe_tick(now)?;
        let suspect_after = self.config.suspect_after.get();
        let dead_after = self.config.dead_after.get();
        let epoch = self.epoch;
        let mut newly_suspected = Vec::new();
        let mut newly_dead = Vec::new();
        for (subject, record) in &mut self.subjects {
            if record.dead_at.is_some() {
                continue;
            }
            let elapsed = now.saturating_sub(record.silence_since());
            if elapsed >= dead_after {
                let judgment = DeadJudgment {
                    subject: *subject,
                    epoch: record.epoch,
                    dead_at: now,
                    incarnation: record.incarnation.get(),
                };
                record.dead_at = Some(now);
                newly_dead.push(judgment);
            } else if elapsed >= suspect_after && record.suspected_at.is_none() {
                record.suspected_at = Some(now);
                newly_suspected.push((*subject, now));
            }
        }
        newly_suspected.sort();
        newly_dead.sort_by_key(|judgment| judgment.subject);
        if !newly_dead.is_empty() {
            self.dead_judgments.extend(newly_dead.iter().copied());
        }
        Ok(SweepReport {
            epoch,
            newly_suspected,
            newly_dead,
        })
    }

    /// Closes the current epoch against `new_fence` and reopens tracking in
    /// the new one.
    ///
    /// Defined epoch-loss semantics (documented contract of this piece):
    ///
    /// - every pending suspicion closes as void — silence from the old
    ///   epoch is not evidence beyond the fence, so no `Suspect` promotes
    ///   to `Dead` across the boundary;
    /// - heartbeat evidence also does not cross: each subject measures
    ///   silence from the epoch-advance tick until it presents a fresh
    ///   heartbeat;
    /// - `Dead` judgments stay immutable history; their subjects re-enter
    ///   tracking as a fresh incarnation (`prior_deaths + 1`), so an old
    ///   incarnation never silently revives.
    ///
    /// # Errors
    ///
    /// Returns [`FailureDetectorError::IdentityMismatch`] when the fence
    /// names a different Cell, and
    /// [`FailureDetectorError::EpochNotAdvanced`] when its epoch does not
    /// strictly advance the current one.
    pub fn on_epoch_advanced(
        &mut self,
        new_fence: &CellFence,
    ) -> Result<EpochAdvanceReport, FailureDetectorError> {
        if new_fence.identity() != self.identity {
            return Err(FailureDetectorError::IdentityMismatch {
                presented: new_fence.identity(),
                current: self.identity,
            });
        }
        if new_fence.epoch() <= self.epoch {
            return Err(FailureDetectorError::EpochNotAdvanced {
                presented: new_fence.epoch(),
                current: self.epoch,
            });
        }
        let previous_epoch = self.epoch;
        let new_epoch = new_fence.epoch();
        let boundary_tick = self.last_tick;
        let mut suspicions_voided = Vec::new();
        let mut re_tracked_after_death = Vec::new();
        for (subject, record) in &mut self.subjects {
            if let Some(suspected_at) = record.suspected_at {
                suspicions_voided.push((*subject, suspected_at));
                record.suspected_at = None;
            }
            if record.dead_at.is_some() {
                record.dead_at = None;
                record.incarnation = record.incarnation.saturating_add(1);
                re_tracked_after_death.push(*subject);
            }
            record.epoch = new_epoch;
            record.baseline_tick = boundary_tick;
            record.last_heartbeat = None;
        }
        suspicions_voided.sort();
        re_tracked_after_death.sort();
        self.epoch = new_epoch;
        Ok(EpochAdvanceReport {
            previous_epoch,
            new_epoch,
            suspicions_voided,
            re_tracked_after_death,
        })
    }

    /// Advances the caller clock, rejecting regressions.
    fn observe_tick(&mut self, now: u64) -> Result<(), FailureDetectorError> {
        if now < self.last_tick {
            return Err(FailureDetectorError::ClockRegression {
                presented: now,
                last_seen: self.last_tick,
            });
        }
        self.last_tick = now;
        Ok(())
    }
}
