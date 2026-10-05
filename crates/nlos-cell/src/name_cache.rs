//! Cell-local capability/name cache — second piece of the §26.1
//! seven-piece set.
//!
//! Scope: one Cell keeps a local mapping from namespace paths to capability
//! handles so name lookups during a control-plane partition stay local
//! (`[DIST-LOCAL-001]`: only already-authorized, provable capability is
//! used). The cache is a *cache*, never a second authority: the durable
//! capability authority (issue/attenuate/revoke, `[CAP-UNFORGE-001]` /
//! `[CAP-REVOKE-001]`) stays wherever the assembly wired it; this module
//! only decides which cached answers are still consistent with the fences
//! the caller reports.
//!
//! Consistency contract (cache coherence, monotone invalidation):
//!
//! - every cached entry carries its source capability's `Generation`; an
//!   entry can never cross its source's generation fence — once a path is
//!   invalidated through generation `G`, entries at `G` or older are dead
//!   for good and can never be served or re-inserted (`[CAP-REVOKE-001]`:
//!   hiding from a directory is not revocation, so invalidation here is
//!   durable and one-way);
//! - invalidation is monotone per path: the invalidated-through high-water
//!   only moves up; a stale (older) invalidation presentation is a typed
//!   reject, never a rollback;
//! - epoch invalidation fences everything: when the Cell epoch advances,
//!   all entries inserted before the boundary become invisible (nothing
//!   cached under an old epoch is served under a new one), while per-path
//!   generation high-waters survive the boundary so stale re-insertion
//!   stays fenced.
//!
//! Name paths follow the namespace shape of v0.5 §5
//! (`[NS-NAME-001]`/`[NS-NOENT-001]`): absolute, non-empty
//! `/`-separated segments. A miss returns `None` — this cache never
//! distinguishes "does not exist globally" from "not in this namespace"
//! (`E_NOENT` honesty belongs to the namespace face, not the cache).
//!
//! This module is pure in-memory single-Cell state: no threads, no
//! persistence, no IPC.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use nlos_types::{CapabilityId, Generation};

use crate::{CellEpoch, CellFence, CellIdentity};

/// One absolute namespace path: `/`-separated, non-empty segments, visible
/// ASCII only. Owned and validated at construction so a malformed path is
/// a typed reject, never a silent key.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NamePath {
    path: String,
}

impl NamePath {
    /// Validates and builds a path from `/segment/...` form.
    ///
    /// # Errors
    ///
    /// Returns [`NameCacheError::InvalidPath`] when the path is empty, is
    /// not absolute (`/`-prefixed), has a trailing separator, has an empty
    /// segment, or contains a byte outside printable ASCII (0x21..=0x7e
    /// plus the `/` separator).
    pub fn new(path: &str) -> Result<Self, NameCacheError> {
        let invalid = || NameCacheError::InvalidPath {
            presented: path.to_string(),
        };
        if !path.starts_with('/') || path.len() < 2 || path.ends_with('/') {
            return Err(invalid());
        }
        if !path
            .bytes()
            .all(|byte| byte == b'/' || (0x21..=0x7e).contains(&byte))
        {
            return Err(invalid());
        }
        if path[1..].split('/').any(str::is_empty) {
            return Err(invalid());
        }
        Ok(Self {
            path: path.to_string(),
        })
    }

    /// Returns the validated path text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.path
    }
}

impl fmt::Display for NamePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.path)
    }
}

/// One cached capability handle: the capability identity plus the source
/// capability's generation fence reported at insert time.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CachedCapability {
    /// Capability identity the path resolves to.
    pub capability: CapabilityId,
    /// Source capability's generation when the entry was cached. Entries
    /// at or below a later invalidation high-water are fenced forever.
    pub capability_generation: Generation,
}

/// A served cache hit: the cached handle plus the epoch it was inserted
/// under (never older than the cache's current epoch).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CacheHit {
    /// The cached capability handle.
    pub cached: CachedCapability,
    /// Epoch the entry was inserted in. A hit is only served when this
    /// equals the cache's current epoch.
    pub inserted_epoch: CellEpoch,
}

/// Outcome of an accepted insert.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InsertOutcome {
    /// No prior entry existed; the entry is now cached.
    Inserted,
    /// An identical entry (same capability and generation) was already
    /// cached; nothing changed (idempotent replay).
    Unchanged,
    /// A live older entry was replaced by the presented newer generation.
    Replaced {
        /// The entry that was evicted.
        previous: CachedCapability,
    },
}

/// Outcome of an accepted invalidation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidationOutcome {
    /// The path's high-water moved up to the presented generation; entries
    /// at or below it are fenced. `evicted` counts entries actually
    /// dropped by this invalidation.
    Advanced {
        /// Invalidation high-water now in force for the path.
        invalidated_through: Generation,
        /// Live entries dropped by this invalidation (0 or 1 per path).
        evicted: u8,
    },
    /// The presented generation equals the current high-water; replay,
    /// nothing changed.
    Idempotent,
}

/// Result of an epoch advance: everything cached before the boundary is
/// fenced at once.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EpochInvalidation {
    /// Epoch the cache served before the advance.
    pub previous_epoch: CellEpoch,
    /// Epoch the cache serves now.
    pub new_epoch: CellEpoch,
    /// Number of live entries fenced by the boundary (they were all
    /// inserted before it).
    pub entries_fenced: usize,
}

/// Errors from validating or mutating the name cache.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NameCacheError {
    /// Path text is not a valid absolute namespace path.
    InvalidPath {
        /// The rejected path text.
        presented: String,
    },
    /// Insert presented a generation at or below the path's invalidation
    /// high-water; fenced entries never come back.
    StaleGeneration {
        /// Path the insert targeted.
        path: NamePath,
        /// Generation on the presented entry.
        presented: Generation,
        /// High-water already in force for the path.
        invalidated_through: Generation,
    },
    /// Insert presented an older generation than the live entry.
    StaleEntry {
        /// Path the insert targeted.
        path: NamePath,
        /// Generation on the presented entry.
        presented: Generation,
        /// Generation of the live entry.
        current: Generation,
    },
    /// Insert presented the same generation as the live entry but a
    /// different capability; one generation of one path cannot resolve to
    /// two handles.
    GenerationConflict {
        /// Path the insert targeted.
        path: NamePath,
        /// Capability on the presented entry.
        presented: CapabilityId,
        /// Capability of the live entry at the same generation.
        current: CapabilityId,
        /// Generation both sides presented.
        generation: Generation,
    },
    /// Invalidation presented a generation below the path's high-water;
    /// invalidation is monotone and cannot roll back.
    StaleInvalidation {
        /// Path the invalidation targeted.
        path: NamePath,
        /// Generation on the presentation.
        presented: Generation,
        /// High-water already in force for the path.
        invalidated_through: Generation,
    },
    /// Epoch advance presented a fence whose epoch does not strictly
    /// advance the cache's current epoch.
    EpochNotAdvanced {
        /// Epoch on the presented fence.
        presented: CellEpoch,
        /// Epoch the cache currently serves.
        current: CellEpoch,
    },
    /// Epoch advance presented a fence of a different Cell identity.
    IdentityMismatch {
        /// Identity on the presented fence.
        presented: CellIdentity,
        /// Identity this cache is bound to.
        current: CellIdentity,
    },
}

impl fmt::Display for NameCacheError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath { presented } => {
                write!(formatter, "invalid namespace path: {presented:?}")
            }
            Self::StaleGeneration {
                path,
                presented,
                invalidated_through,
            } => write!(
                formatter,
                "insert into {path} at generation {} is fenced: invalidated through {}",
                presented.get(),
                invalidated_through.get()
            ),
            Self::StaleEntry {
                path,
                presented,
                current,
            } => write!(
                formatter,
                "stale entry for {path}: presented {} < live {}",
                presented.get(),
                current.get()
            ),
            Self::GenerationConflict {
                path,
                presented,
                current,
                generation,
            } => write!(
                formatter,
                "generation conflict on {path} at generation {}: presented {presented:?} != live {current:?}",
                generation.get()
            ),
            Self::StaleInvalidation {
                path,
                presented,
                invalidated_through,
            } => write!(
                formatter,
                "stale invalidation for {path}: presented {} < high-water {}",
                presented.get(),
                invalidated_through.get()
            ),
            Self::EpochNotAdvanced { presented, current } => write!(
                formatter,
                "name-cache epoch must strictly advance: presented {} <= current {}",
                presented.get(),
                current.get()
            ),
            Self::IdentityMismatch { presented, current } => write!(
                formatter,
                "name-cache identity mismatch: presented {presented:?} != current {current:?}"
            ),
        }
    }
}

impl Error for NameCacheError {}

/// Per-path cache state: at most one live entry plus the monotone
/// invalidation high-water that survives the entry and epoch boundaries.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PathState {
    live: Option<CachedCapability>,
    inserted_epoch: Option<CellEpoch>,
    invalidated_through: Option<Generation>,
}

/// Cell-local capability/name cache bound to one Cell identity and epoch.
///
/// Construct it from the authority's current fence snapshot; it never owns
/// or claims the authority itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityNameCache {
    identity: CellIdentity,
    epoch: CellEpoch,
    paths: HashMap<NamePath, PathState>,
}

impl CapabilityNameCache {
    /// Binds a cache to the Cell fence snapshot.
    #[must_use]
    pub fn new(fence: &CellFence) -> Self {
        Self {
            identity: fence.identity(),
            epoch: fence.epoch(),
            paths: HashMap::new(),
        }
    }

    /// Identity this cache is bound to.
    #[must_use]
    pub const fn identity(&self) -> CellIdentity {
        self.identity
    }

    /// Epoch this cache currently serves.
    #[must_use]
    pub const fn epoch(&self) -> CellEpoch {
        self.epoch
    }

    /// Number of paths the cache tracks (live or tombstoned).
    #[must_use]
    pub fn path_count(&self) -> usize {
        self.paths.len()
    }

    /// Invalidation high-water in force for `path`, or `None` when the
    /// path was never invalidated (including never seen).
    #[must_use]
    pub fn invalidated_through(&self, path: &NamePath) -> Option<Generation> {
        self.paths
            .get(path)
            .and_then(|state| state.invalidated_through)
    }

    /// Serves the cached handle for `path`, if a live entry survives both
    /// fences (generation high-water and current epoch).
    ///
    /// A miss is `None` regardless of cause (absent, invalidated, or
    /// fenced by an epoch boundary): this cache never leaks whether an
    /// object exists elsewhere (`[NS-NOENT-001]`).
    #[must_use]
    pub fn get(&self, path: &NamePath) -> Option<CacheHit> {
        let state = self.paths.get(path)?;
        let cached = state.live?;
        let inserted_epoch = state.inserted_epoch?;
        if inserted_epoch != self.epoch {
            return None;
        }
        Some(CacheHit {
            cached,
            inserted_epoch,
        })
    }

    /// Caches `capability` at `capability_generation` for `path`.
    ///
    /// The presented generation must clear the path's invalidation
    /// high-water (fenced entries never come back), and must not be older
    /// than a live entry. An identical replay is idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`NameCacheError::StaleGeneration`] when the presented
    /// generation is at or below the path's invalidation high-water,
    /// [`NameCacheError::StaleEntry`] when it is older than the live
    /// entry, and [`NameCacheError::GenerationConflict`] when the same
    /// generation presents a different capability than the live entry.
    pub fn insert(
        &mut self,
        path: NamePath,
        capability: CapabilityId,
        capability_generation: Generation,
    ) -> Result<InsertOutcome, NameCacheError> {
        let state = self.paths.entry(path.clone()).or_insert(PathState {
            live: None,
            inserted_epoch: None,
            invalidated_through: None,
        });
        if let Some(invalidated_through) = state.invalidated_through
            && capability_generation <= invalidated_through
        {
            return Err(NameCacheError::StaleGeneration {
                path,
                presented: capability_generation,
                invalidated_through,
            });
        }
        let presented = CachedCapability {
            capability,
            capability_generation,
        };
        if let Some(current) = state.live {
            if capability_generation < current.capability_generation {
                return Err(NameCacheError::StaleEntry {
                    path,
                    presented: capability_generation,
                    current: current.capability_generation,
                });
            }
            if capability_generation == current.capability_generation {
                if current.capability == capability {
                    return Ok(InsertOutcome::Unchanged);
                }
                return Err(NameCacheError::GenerationConflict {
                    path,
                    presented: capability,
                    current: current.capability,
                    generation: capability_generation,
                });
            }
            state.live = Some(presented);
            state.inserted_epoch = Some(self.epoch);
            Ok(InsertOutcome::Replaced { previous: current })
        } else {
            state.live = Some(presented);
            state.inserted_epoch = Some(self.epoch);
            Ok(InsertOutcome::Inserted)
        }
    }

    /// Advances the invalidation high-water of `path` through
    /// `through`: every entry whose source generation is at or below
    /// `through` is fenced permanently (monotone, irreversible).
    ///
    /// Invalidating an unseen path is accepted: it installs the high-water
    /// so later inserts at or below `through` are fenced pre-emptively.
    ///
    /// # Errors
    ///
    /// Returns [`NameCacheError::StaleInvalidation`] when `through` is
    /// below the path's current high-water (invalidation cannot roll
    /// back).
    pub fn invalidate(
        &mut self,
        path: NamePath,
        through: Generation,
    ) -> Result<InvalidationOutcome, NameCacheError> {
        let state = self.paths.entry(path.clone()).or_insert(PathState {
            live: None,
            inserted_epoch: None,
            invalidated_through: None,
        });
        match state.invalidated_through {
            Some(current) if through < current => Err(NameCacheError::StaleInvalidation {
                path,
                presented: through,
                invalidated_through: current,
            }),
            Some(current) if through == current => Ok(InvalidationOutcome::Idempotent),
            _ => {
                state.invalidated_through = Some(through);
                let mut evicted = 0;
                if let Some(live) = state.live
                    && live.capability_generation <= through
                {
                    state.live = None;
                    state.inserted_epoch = None;
                    evicted = 1;
                }
                Ok(InvalidationOutcome::Advanced {
                    invalidated_through: through,
                    evicted,
                })
            }
        }
    }

    /// Fences every entry cached before the epoch boundary.
    ///
    /// All live entries become invisible (their insert epoch is older than
    /// the new one); per-path generation high-waters survive so stale
    /// re-insertion stays fenced across epoch boundaries. Re-presenting a
    /// non-advancing or foreign fence is a typed reject.
    ///
    /// # Errors
    ///
    /// Returns [`NameCacheError::IdentityMismatch`] when the fence names a
    /// different Cell, and [`NameCacheError::EpochNotAdvanced`] when its
    /// epoch does not strictly advance the current one.
    pub fn on_epoch_advanced(
        &mut self,
        new_fence: &CellFence,
    ) -> Result<EpochInvalidation, NameCacheError> {
        if new_fence.identity() != self.identity {
            return Err(NameCacheError::IdentityMismatch {
                presented: new_fence.identity(),
                current: self.identity,
            });
        }
        if new_fence.epoch() <= self.epoch {
            return Err(NameCacheError::EpochNotAdvanced {
                presented: new_fence.epoch(),
                current: self.epoch,
            });
        }
        let previous_epoch = self.epoch;
        let new_epoch = new_fence.epoch();
        let mut entries_fenced = 0;
        for state in self.paths.values_mut() {
            if state.live.take().is_some() {
                state.inserted_epoch = None;
                entries_fenced += 1;
            }
        }
        self.epoch = new_epoch;
        Ok(EpochInvalidation {
            previous_epoch,
            new_epoch,
            entries_fenced,
        })
    }
}
