//! Process-scoped Cell identity / epoch / fencing (C-CELL first slice).
//!
//! ADR-0018: one OS process is one Cell authority. This crate does not
//! implement the Cell-local seven-piece set, consensus, or any transport.

use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;
use std::sync::Mutex;

use nlos_types::{Generation, SchedulerDomainId};

static PROCESS_CLAIM: Mutex<Option<CellIdentity>> = Mutex::new(None);

/// Stable Cell identity: the Cell layer of [`SchedulerDomainId`].
///
/// The bytes are opaque and must not encode host, path, pid, or port
/// (`MODEL-ID-002`, `DIST-NAME-001`). v0.5 §33 does not list a `CellId`
/// nominal type; this slice reuses the existing Cell-layer domain id
/// rather than inventing one.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CellIdentity {
    domain: SchedulerDomainId,
}

impl CellIdentity {
    /// Wraps an existing stable domain id as a Cell identity.
    #[must_use]
    pub const fn from_domain(domain: SchedulerDomainId) -> Self {
        Self { domain }
    }

    /// Returns the stable domain id.
    #[must_use]
    pub const fn domain(self) -> SchedulerDomainId {
        self.domain
    }

    /// Returns the opaque identity bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        self.domain.as_bytes()
    }
}

/// Monotonic Cell control-plane epoch. Distinct from [`Generation`]
/// (`node_boot_generation`) and from fencing tokens.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CellEpoch(NonZeroU64);

impl CellEpoch {
    /// First epoch issued at process claim.
    pub const INITIAL: Self = Self(NonZeroU64::MIN);

    /// Rebuilds an epoch from its counter form. `0` is not an epoch.
    #[must_use]
    pub const fn from_u64(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the counter form.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// Next epoch, or `None` if the space is exhausted.
    #[must_use]
    pub fn checked_next(self) -> Option<Self> {
        self.0
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .map(Self)
    }
}

/// Monotonic fencing token inside one Cell fence scope.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CellFencingToken(NonZeroU64);

impl CellFencingToken {
    /// First token issued at process claim.
    pub const INITIAL: Self = Self(NonZeroU64::MIN);

    /// Rebuilds a token from its counter form. `0` is not a token.
    #[must_use]
    pub const fn from_u64(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the counter form.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// Next token, or `None` if the space is exhausted.
    #[must_use]
    pub fn checked_next(self) -> Option<Self> {
        self.0
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .map(Self)
    }
}

/// Presented fence: identity + boot generation + epoch + token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CellFence {
    identity: CellIdentity,
    node_boot_generation: Generation,
    epoch: CellEpoch,
    fencing_token: CellFencingToken,
}

impl CellFence {
    /// Builds a fence presentation. Callers supply the four axes explicitly;
    /// this is not a transport decoder.
    #[must_use]
    pub const fn present(
        identity: CellIdentity,
        node_boot_generation: Generation,
        epoch: CellEpoch,
        fencing_token: CellFencingToken,
    ) -> Self {
        Self {
            identity,
            node_boot_generation,
            epoch,
            fencing_token,
        }
    }

    /// Stable identity on the presentation.
    #[must_use]
    pub const fn identity(self) -> CellIdentity {
        self.identity
    }

    /// Boot generation on the presentation.
    #[must_use]
    pub const fn node_boot_generation(self) -> Generation {
        self.node_boot_generation
    }

    /// Epoch on the presentation.
    #[must_use]
    pub const fn epoch(self) -> CellEpoch {
        self.epoch
    }

    /// Fencing token on the presentation.
    #[must_use]
    pub const fn fencing_token(self) -> CellFencingToken {
        self.fencing_token
    }
}

/// Process-scoped Cell authority. Not cloneable: a second in-process
/// instance is not a second Cell (ADR-0018).
#[derive(Debug)]
pub struct CellAuthority {
    identity: CellIdentity,
    node_boot_generation: Generation,
    epoch: CellEpoch,
    fencing_token: CellFencingToken,
    os_process_id: u32,
}

impl CellAuthority {
    /// Claims the unique Cell authority for this OS process.
    ///
    /// # Errors
    ///
    /// Returns [`CellError::AlreadyClaimedInProcess`] when this process
    /// already holds a Cell. A second in-process claim is not a second Cell.
    pub fn claim(domain: SchedulerDomainId) -> Result<Self, CellError> {
        let identity = CellIdentity::from_domain(domain);
        let mut slot = PROCESS_CLAIM.lock().map_err(|_| CellError::LockPoisoned)?;
        if let Some(existing) = *slot {
            return Err(CellError::AlreadyClaimedInProcess { existing });
        }
        *slot = Some(identity);
        drop(slot);
        Ok(Self {
            identity,
            node_boot_generation: Generation::INITIAL,
            epoch: CellEpoch::INITIAL,
            fencing_token: CellFencingToken::INITIAL,
            os_process_id: std::process::id(),
        })
    }

    /// Stable identity. Does not include the OS pid.
    #[must_use]
    pub const fn identity(&self) -> CellIdentity {
        self.identity
    }

    /// Process boot generation for this claim.
    #[must_use]
    pub const fn node_boot_generation(&self) -> Generation {
        self.node_boot_generation
    }

    /// Current epoch.
    #[must_use]
    pub const fn epoch(&self) -> CellEpoch {
        self.epoch
    }

    /// Current fencing token.
    #[must_use]
    pub const fn fencing_token(&self) -> CellFencingToken {
        self.fencing_token
    }

    /// OS process that holds this authority. Observational only; not part of
    /// the stable identity.
    #[must_use]
    pub const fn os_process_id(&self) -> u32 {
        self.os_process_id
    }

    /// Snapshot of the current fence.
    #[must_use]
    pub const fn fence(&self) -> CellFence {
        CellFence::present(
            self.identity,
            self.node_boot_generation,
            self.epoch,
            self.fencing_token,
        )
    }

    /// Advances epoch and fencing token together.
    ///
    /// # Errors
    ///
    /// Returns [`CellError::GenerationExhausted`] when either counter cannot
    /// advance.
    pub fn advance_epoch(&mut self) -> Result<CellFence, CellError> {
        let epoch = self
            .epoch
            .checked_next()
            .ok_or(CellError::GenerationExhausted)?;
        let fencing_token = self
            .fencing_token
            .checked_next()
            .ok_or(CellError::GenerationExhausted)?;
        self.epoch = epoch;
        self.fencing_token = fencing_token;
        Ok(self.fence())
    }

    /// Admits a presented fence against the current authority.
    ///
    /// Fail-closed: identity, boot generation, epoch, and token must all
    /// match. A lower epoch is [`CellAdmitError::StaleEpoch`].
    ///
    /// # Errors
    ///
    /// Typed reject: [`CellAdmitError`].
    pub fn admit(&self, presented: &CellFence) -> Result<(), CellAdmitError> {
        if presented.identity != self.identity {
            return Err(CellAdmitError::IdentityMismatch {
                presented: presented.identity,
                current: self.identity,
            });
        }
        if presented.node_boot_generation != self.node_boot_generation {
            return Err(CellAdmitError::BootGenerationMismatch {
                presented: presented.node_boot_generation,
                current: self.node_boot_generation,
            });
        }
        if presented.epoch < self.epoch {
            return Err(CellAdmitError::StaleEpoch {
                presented: presented.epoch,
                current: self.epoch,
            });
        }
        if presented.epoch != self.epoch {
            return Err(CellAdmitError::EpochMismatch {
                presented: presented.epoch,
                current: self.epoch,
            });
        }
        if presented.fencing_token != self.fencing_token {
            return Err(CellAdmitError::FencingTokenMismatch {
                presented: presented.fencing_token,
                current: self.fencing_token,
            });
        }
        Ok(())
    }
}

/// Errors from claiming or advancing a Cell authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CellError {
    /// This OS process already holds a Cell; in-proc dual instance is forbidden.
    AlreadyClaimedInProcess {
        /// Identity of the Cell already claimed in this process.
        existing: CellIdentity,
    },
    /// Epoch or fencing-token space is exhausted; fail closed.
    GenerationExhausted,
    /// Process claim lock was poisoned.
    LockPoisoned,
}

impl fmt::Display for CellError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyClaimedInProcess { existing } => {
                write!(
                    formatter,
                    "this OS process already holds Cell {existing:?}; in-process dual Cell is forbidden"
                )
            }
            Self::GenerationExhausted => {
                formatter.write_str("Cell epoch or fencing token space exhausted")
            }
            Self::LockPoisoned => formatter.write_str("Cell process-claim lock poisoned"),
        }
    }
}

impl Error for CellError {}

/// Typed fail-closed rejects for a presented fence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CellAdmitError {
    /// Presented epoch is older than the authority's current epoch.
    StaleEpoch {
        /// Epoch on the presentation.
        presented: CellEpoch,
        /// Epoch currently held by the authority.
        current: CellEpoch,
    },
    /// Presented epoch is not the current epoch and is not older (fail closed).
    EpochMismatch {
        /// Epoch on the presentation.
        presented: CellEpoch,
        /// Epoch currently held by the authority.
        current: CellEpoch,
    },
    /// Presented boot generation is not this process claim's generation.
    BootGenerationMismatch {
        /// Boot generation on the presentation.
        presented: Generation,
        /// Boot generation currently held by the authority.
        current: Generation,
    },
    /// Presented identity is not this Cell.
    IdentityMismatch {
        /// Identity on the presentation.
        presented: CellIdentity,
        /// Identity of this authority.
        current: CellIdentity,
    },
    /// Epoch matches but the fencing token does not.
    FencingTokenMismatch {
        /// Token on the presentation.
        presented: CellFencingToken,
        /// Token currently held by the authority.
        current: CellFencingToken,
    },
}

impl fmt::Display for CellAdmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleEpoch { presented, current } => write!(
                formatter,
                "stale Cell epoch: presented {} < current {}",
                presented.get(),
                current.get()
            ),
            Self::EpochMismatch { presented, current } => write!(
                formatter,
                "Cell epoch mismatch: presented {} != current {}",
                presented.get(),
                current.get()
            ),
            Self::BootGenerationMismatch { presented, current } => write!(
                formatter,
                "Cell boot generation mismatch: presented {} != current {}",
                presented.get(),
                current.get()
            ),
            Self::IdentityMismatch { presented, current } => write!(
                formatter,
                "Cell identity mismatch: presented {presented:?} != current {current:?}"
            ),
            Self::FencingTokenMismatch { presented, current } => write!(
                formatter,
                "Cell fencing token mismatch: presented {} != current {}",
                presented.get(),
                current.get()
            ),
        }
    }
}

impl Error for CellAdmitError {}
