//! Typed failure surface of the Slice K longitudinal assembly. Every
//! variant wraps one landed authority's typed error verbatim — the slice
//! adds no semantics of its own, it only names which authority refused.

use std::error::Error;
use std::fmt;

use nlos_application::ApplicationAuthorityError;
use nlos_artifact::ArtifactError;
use nlos_capability::CapabilityAuthorityError;
use nlos_cell::{CellError, FailureDetectorError, NameCacheError};
use nlos_channel::ChannelAuthorityError;
use nlos_clock::AuthorityClockError;
use nlos_commit_coordinator::CoordinatorError;
use nlos_driver_mock::ProviderError;
use nlos_identity::IdentityAuthorityError;
use nlos_lease::{QuotaLeaseGrantError, QuotaLeaseLedgerError};
use nlos_plan::PlanStoreError;
use nlos_process::ProcessAuthorityError;
use nlos_runtime::RuntimeError;
use nlos_runtime_tokio::ChannelWaitError;
use nlos_semantic::SemanticAuthorityError;
use nlos_store::StoreError;
use nlos_task::TaskStoreError;
use nlos_topic::TopicAuthorityError;

use crate::cell_host::{CellBroadcastRefusal, CellEpochBroadcastIncomplete};

/// Fail-closed errors of the Slice K assembly.
#[derive(Debug)]
pub enum SliceKError {
    /// Filesystem failure while creating the runtime root.
    Io(std::io::Error),
    /// The identity authority refused a bootstrap or verification readback.
    Identity(IdentityAuthorityError),
    /// The process authority refused a binding/domain materialization step.
    Process(ProcessAuthorityError),
    /// The artifact authority refused a store or package step.
    Artifact(ArtifactError),
    /// The application authority refused an installation step.
    Application(ApplicationAuthorityError),
    /// The task authority refused a task/attempt/permit/plan step.
    Task(TaskStoreError),
    /// The plan authority refused a declaration step of the install→plan
    /// head segment (W60): a structurally invalid revision (cycle, bound,
    /// frozen-shape rewrite), an apply-time declaration-admission denial
    /// or consult failure, an idempotency rebinding, or a storage
    /// failure. When this surfaces from the templated install path after
    /// the application authority already committed, the installation fact
    /// is retained and only the plan revision is missing — see the window
    /// semantics on the install wiring in [`crate::package`].
    Plan(PlanStoreError),
    /// The install→plan wiring refused a durable state it cannot proceed
    /// on honestly (W60): a verification receipt that does not bind the
    /// presented signed task-templated package (refused before any
    /// durable write), or an update-generation install whose application
    /// has no plan genesis revision in the plan authority (legacy
    /// install, install-window residue, or a replaced plan store — the
    /// installation itself is already committed in that case; converge by
    /// replaying the application's generation-1 templated install).
    InstallPlanState(&'static str),
    /// The clock authority refused a reading.
    Clock(AuthorityClockError),
    /// The capability authority refused an open, issuance, delegation, or
    /// admission step.
    Capability(CapabilityAuthorityError),
    /// The semantic authority refused an admission the production write
    /// bridge attempted (canonical, signature, execution-fence,
    /// capability, content, or lineage gate), or its store-signing step
    /// failed. The payload lane treats this as fail-closed: a run whose
    /// terminal receipt the semantic ledger did not record is reported as
    /// an error, never as success.
    Semantic(SemanticAuthorityError),
    /// The semantic writer's durable key material is unusable (for example
    /// a `semantic-writer.key` whose width is not the documented 48-byte
    /// `seed ‖ valid_from ‖ valid_until` layout).
    SemanticWriter(&'static str),
    /// The system Channel endpoint authority refused a semantic-stream
    /// bootstrap step (the durable queue under `<root>/channel`; distinct
    /// from the runtime's fiber wake `ChannelWaitError` lane).
    Channel(ChannelAuthorityError),
    /// The Topic service-layer authority refused a semantic-stream step
    /// (bootstrap, publish, cursor advance, or compact). Channel
    /// rejections propagated through the topic layer stay reachable via
    /// [`std::error::Error::source`].
    Topic(TopicAuthorityError),
    /// The runtime's Outbox pump lifecycle refused a transition this call
    /// cannot make honestly (for example starting a second pump while one
    /// is still running — its wake lane would silently ack the first
    /// lane's wakes as `FiberGone`).
    Pump(&'static str),
    /// The operation store refused a driver-operation step.
    Operation(StoreError),
    /// The driver-mock provider face refused a payload-execution step
    /// (unreachable RPC or a durable operation-authority rejection such as
    /// a callback-identity conflict on a mutated payload).
    Driver(ProviderError),
    /// The payload-execution lane refused a durable state this call cannot
    /// execute: no application under the package identity, a non-installed
    /// status, a missing installation receipt, or an executable entry that
    /// was never materialized into the artifact authority.
    PayloadState(&'static str),
    /// The supervisor pid registry refused a registration (second-process
    /// kill chain spawn phase).
    SupervisorPid(nlos_process::SupervisorPidRegistryError),
    /// The runtime batch-cancel linkage refused the propagation
    /// (second-process kill chain).
    BatchCancel(ChannelWaitError),
    /// The tokio runtime adapter refused a fiber admission or cancel.
    Runtime(RuntimeError),
    /// The cross-authority commit coordinator refused a convergence step.
    Coordinator(CoordinatorError),
    /// The teardown lane refused a durable state this chain never produces
    /// (e.g. a binding terminal without a platform-kill receipt — an
    /// out-of-band crash): the assembly names the refusal instead of
    /// guessing a transition the authorities do not offer.
    TeardownState(&'static str),
    /// The crash-revival lane refused a durable state this chain never
    /// produces (e.g. a terminal marker at a generation the caller's
    /// crashed binding does not name, or a head that is neither the
    /// crashed generation nor its direct revival): the assembly names the
    /// refusal instead of guessing a transition the authorities do not
    /// offer.
    RevivalState(&'static str),
    /// The Cell authority refused a claim or an epoch advance of the cell
    /// host assembly (W48): an in-process second claim, an unusable cell
    /// data directory, or an exhausted epoch/fencing space.
    Cell(CellError),
    /// The cell host's quota lease sub-ledger refused a grant (W48): a
    /// stale or mismatched presented fence, insufficient `AVAILABLE`, or a
    /// conflicting replay of the same lease id.
    Lease(QuotaLeaseGrantError),
    /// The cell host's quota lease sub-ledger refused a ledger read (W48):
    /// an unknown lease id.
    LeaseLedger(QuotaLeaseLedgerError),
    /// The cell-local failure detector refused a heartbeat or sweep the
    /// cell host drove (W48): a logical-clock regression, or a heartbeat
    /// for an incarnation already judged dead.
    FailureDetector(FailureDetectorError),
    /// The cell-local capability/name cache refused an insert or
    /// invalidation through the cell host (W48): a fenced or stale
    /// generation, a same-generation conflict, or a rollback presentation.
    NameCache(NameCacheError),
    /// The cell host refused an assembly state it cannot proceed on
    /// honestly (W48; currently only heartbeat tick-sequence exhaustion).
    CellHost(&'static str),
    /// The Cell epoch advanced but a snapshot consumer's broadcast refused
    /// (W48): the typed incomplete state naming which consumer already
    /// received the fence — an advanced-but-partially-broadcast epoch is
    /// never silently swallowed. Boxed: the report payload is large and
    /// this arm is the unreachable-by-construction tail of the enum.
    CellBroadcast(Box<CellEpochBroadcastIncomplete>),
    /// The system-control prefix refused an NL command (out-of-grammar
    /// sentence or dispatch-contract defect); handler rejections surface
    /// as typed receipt failures inside
    /// [`ControlReceipt::outcome`](nlos_system_control::control::ControlReceipt::outcome),
    /// not here.
    Control(nlos_system_control::control::ControlError),
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
            Self::Process(error) => write!(formatter, "process authority: {error}"),
            Self::Artifact(error) => write!(formatter, "artifact authority: {error}"),
            Self::Application(error) => write!(formatter, "application authority: {error}"),
            Self::Task(error) => write!(formatter, "task authority: {error}"),
            Self::Plan(error) => write!(formatter, "plan authority: {error}"),
            Self::InstallPlanState(reason) => {
                write!(formatter, "install-to-plan wiring state refusal: {reason}")
            }
            Self::Clock(error) => write!(formatter, "clock authority: {error}"),
            Self::Capability(error) => write!(formatter, "capability authority: {error}"),
            Self::Semantic(error) => write!(formatter, "semantic authority: {error}"),
            Self::SemanticWriter(reason) => {
                write!(formatter, "semantic writer key material refusal: {reason}")
            }
            Self::Channel(error) => {
                write!(formatter, "system channel authority: {error}")
            }
            Self::Topic(error) => {
                write!(formatter, "topic service authority: {error}")
            }
            Self::Pump(reason) => write!(formatter, "outbox pump lifecycle refusal: {reason}"),
            Self::Operation(error) => write!(formatter, "operation store: {error}"),
            Self::Driver(error) => write!(formatter, "driver provider face: {error}"),
            Self::PayloadState(reason) => {
                write!(formatter, "payload execution state refusal: {reason}")
            }
            Self::SupervisorPid(error) => {
                write!(formatter, "supervisor pid registry: {error}")
            }
            Self::BatchCancel(error) => {
                write!(formatter, "runtime batch-cancel linkage: {error}")
            }
            Self::Runtime(error) => write!(formatter, "fiber runtime: {error}"),
            Self::Coordinator(error) => write!(formatter, "commit coordinator: {error}"),
            Self::TeardownState(reason) => {
                write!(formatter, "teardown state refusal: {reason}")
            }
            Self::RevivalState(reason) => {
                write!(formatter, "crash-revival state refusal: {reason}")
            }
            Self::Cell(error) => write!(formatter, "cell authority: {error}"),
            Self::Lease(error) => write!(formatter, "quota lease grantor: {error}"),
            Self::LeaseLedger(error) => write!(formatter, "quota lease ledger: {error}"),
            Self::FailureDetector(error) => write!(formatter, "cell failure detector: {error}"),
            Self::NameCache(error) => write!(formatter, "cell name cache: {error}"),
            Self::CellHost(reason) => write!(formatter, "cell host state refusal: {reason}"),
            Self::CellBroadcast(incomplete) => write!(
                formatter,
                "cell epoch advanced to epoch {} token {} but the broadcast refused ({}); incomplete state carried in full",
                incomplete.advanced_to.epoch().get(),
                incomplete.advanced_to.fencing_token().get(),
                incomplete.refusal
            ),
            Self::Control(error) => write!(formatter, "system-control prefix: {error}"),
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
            Self::Process(error) => Some(error),
            Self::Artifact(error) => Some(error),
            Self::Application(error) => Some(error),
            Self::Task(error) => Some(error),
            Self::Plan(error) => Some(error),
            Self::Clock(error) => Some(error),
            Self::Operation(error) => Some(error),
            Self::Driver(error) => Some(error),
            Self::PayloadState(_)
            | Self::TeardownState(_)
            | Self::RevivalState(_)
            | Self::TimestampOverflow(_)
            | Self::SizeOverflow(_)
            | Self::Pump(_)
            | Self::SemanticWriter(_)
            | Self::InstallPlanState(_)
            | Self::CellHost(_) => None,
            Self::SupervisorPid(error) => Some(error),
            Self::BatchCancel(error) => Some(error),
            Self::Runtime(error) => Some(error),
            Self::Coordinator(error) => Some(error),
            Self::Control(error) => Some(error),
            Self::Capability(error) => Some(error),
            Self::Semantic(error) => Some(error),
            Self::Channel(error) => Some(error),
            Self::Topic(error) => Some(error),
            Self::Cell(error) => Some(error),
            Self::Lease(error) => Some(error),
            Self::LeaseLedger(error) => Some(error),
            Self::FailureDetector(error) => Some(error),
            Self::NameCache(error) => Some(error),
            Self::CellBroadcast(incomplete) => match &incomplete.refusal {
                CellBroadcastRefusal::Detector(error) => Some(error),
                CellBroadcastRefusal::Cache(error) => Some(error),
            },
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

impl From<ProcessAuthorityError> for SliceKError {
    fn from(error: ProcessAuthorityError) -> Self {
        Self::Process(error)
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

impl From<PlanStoreError> for SliceKError {
    fn from(error: PlanStoreError) -> Self {
        Self::Plan(error)
    }
}

impl From<AuthorityClockError> for SliceKError {
    fn from(error: AuthorityClockError) -> Self {
        Self::Clock(error)
    }
}

impl From<CapabilityAuthorityError> for SliceKError {
    fn from(error: CapabilityAuthorityError) -> Self {
        Self::Capability(error)
    }
}

impl From<SemanticAuthorityError> for SliceKError {
    fn from(error: SemanticAuthorityError) -> Self {
        Self::Semantic(error)
    }
}

impl From<ChannelAuthorityError> for SliceKError {
    fn from(error: ChannelAuthorityError) -> Self {
        Self::Channel(error)
    }
}

impl From<TopicAuthorityError> for SliceKError {
    fn from(error: TopicAuthorityError) -> Self {
        Self::Topic(error)
    }
}

impl From<StoreError> for SliceKError {
    fn from(error: StoreError) -> Self {
        Self::Operation(error)
    }
}

impl From<ProviderError> for SliceKError {
    fn from(error: ProviderError) -> Self {
        Self::Driver(error)
    }
}

impl From<nlos_process::SupervisorPidRegistryError> for SliceKError {
    fn from(error: nlos_process::SupervisorPidRegistryError) -> Self {
        Self::SupervisorPid(error)
    }
}

impl From<ChannelWaitError> for SliceKError {
    fn from(error: ChannelWaitError) -> Self {
        Self::BatchCancel(error)
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

impl From<CellError> for SliceKError {
    fn from(error: CellError) -> Self {
        Self::Cell(error)
    }
}

impl From<QuotaLeaseGrantError> for SliceKError {
    fn from(error: QuotaLeaseGrantError) -> Self {
        Self::Lease(error)
    }
}

impl From<QuotaLeaseLedgerError> for SliceKError {
    fn from(error: QuotaLeaseLedgerError) -> Self {
        Self::LeaseLedger(error)
    }
}

impl From<FailureDetectorError> for SliceKError {
    fn from(error: FailureDetectorError) -> Self {
        Self::FailureDetector(error)
    }
}

impl From<NameCacheError> for SliceKError {
    fn from(error: NameCacheError) -> Self {
        Self::NameCache(error)
    }
}

/// Result alias for the Slice K assembly.
pub type SliceKResult<T> = Result<T, SliceKError>;
