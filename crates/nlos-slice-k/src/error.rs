//! Typed failure surface of the Slice K longitudinal assembly. Every
//! variant wraps one landed authority's typed error verbatim — the slice
//! adds no semantics of its own, it only names which authority refused.

use std::error::Error;
use std::fmt;

use nlos_application::ApplicationAuthorityError;
use nlos_artifact::ArtifactError;
use nlos_clock::AuthorityClockError;
use nlos_commit_coordinator::CoordinatorError;
use nlos_identity::IdentityAuthorityError;
use nlos_runtime::RuntimeError;
use nlos_store::StoreError;
use nlos_task::TaskStoreError;

/// Fail-closed errors of the Slice K assembly.
#[derive(Debug)]
pub enum SliceKError {
    /// Filesystem failure while creating the runtime root.
    Io(std::io::Error),
    /// The identity authority refused a bootstrap or verification readback.
    Identity(IdentityAuthorityError),
    /// The artifact authority refused a store or package step.
    Artifact(ArtifactError),
    /// The application authority refused an installation step.
    Application(ApplicationAuthorityError),
    /// The task authority refused a task/attempt/permit/plan step.
    Task(TaskStoreError),
    /// The clock authority refused a reading.
    Clock(AuthorityClockError),
    /// The operation store refused a driver-operation step.
    Operation(StoreError),
    /// The tokio runtime adapter refused a fiber admission or cancel.
    Runtime(RuntimeError),
    /// The cross-authority commit coordinator refused a convergence step.
    Coordinator(CoordinatorError),
    /// A wall-clock millisecond value does not fit the callee's `i64`
    /// timestamp domain.
    TimestampOverflow(u64),
    /// A byte-slice length does not fit the callee's `u64` size domain.
    SizeOverflow(usize),
}

impl fmt::Display for SliceKError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "slice-k root I/O failure: {error}"),
            Self::Identity(error) => write!(formatter, "identity authority: {error}"),
            Self::Artifact(error) => write!(formatter, "artifact authority: {error}"),
            Self::Application(error) => write!(formatter, "application authority: {error}"),
            Self::Task(error) => write!(formatter, "task authority: {error}"),
            Self::Clock(error) => write!(formatter, "clock authority: {error}"),
            Self::Operation(error) => write!(formatter, "operation store: {error}"),
            Self::Runtime(error) => write!(formatter, "fiber runtime: {error}"),
            Self::Coordinator(error) => write!(formatter, "commit coordinator: {error}"),
            Self::TimestampOverflow(value) => {
                write!(
                    formatter,
                    "wall ms {value} does not fit the i64 timestamp domain"
                )
            }
            Self::SizeOverflow(length) => {
                write!(
                    formatter,
                    "byte length {length} does not fit the u64 size domain"
                )
            }
        }
    }
}

impl Error for SliceKError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Identity(error) => Some(error),
            Self::Artifact(error) => Some(error),
            Self::Application(error) => Some(error),
            Self::Task(error) => Some(error),
            Self::Clock(error) => Some(error),
            Self::Operation(error) => Some(error),
            Self::Runtime(error) => Some(error),
            Self::Coordinator(error) => Some(error),
            Self::TimestampOverflow(_) | Self::SizeOverflow(_) => None,
        }
    }
}

impl From<std::io::Error> for SliceKError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<IdentityAuthorityError> for SliceKError {
    fn from(error: IdentityAuthorityError) -> Self {
        Self::Identity(error)
    }
}

impl From<ArtifactError> for SliceKError {
    fn from(error: ArtifactError) -> Self {
        Self::Artifact(error)
    }
}

impl From<ApplicationAuthorityError> for SliceKError {
    fn from(error: ApplicationAuthorityError) -> Self {
        Self::Application(error)
    }
}

impl From<TaskStoreError> for SliceKError {
    fn from(error: TaskStoreError) -> Self {
        Self::Task(error)
    }
}

impl From<AuthorityClockError> for SliceKError {
    fn from(error: AuthorityClockError) -> Self {
        Self::Clock(error)
    }
}

impl From<StoreError> for SliceKError {
    fn from(error: StoreError) -> Self {
        Self::Operation(error)
    }
}

impl From<RuntimeError> for SliceKError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

impl From<CoordinatorError> for SliceKError {
    fn from(error: CoordinatorError) -> Self {
        Self::Coordinator(error)
    }
}

/// Result alias for the Slice K assembly.
pub type SliceKResult<T> = Result<T, SliceKError>;
