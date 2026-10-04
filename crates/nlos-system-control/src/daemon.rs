//! Resident `system_control` daemon (`daemon` feature, cross-platform).
//!
//! Decision D5: this module closes the assembly gap between the desktop GUI
//! authenticated entry and the CLI plain entry when no desktop service
//! process is running. One daemon owns a state root, opens every real
//! authority under it (identity, clock, task, artifact, semantic, process,
//! resource, application, plan, channel+topic, operation store, and a tokio
//! runtime adapter), starts the [`TaskAuthorityCommitRecoveryWorker`] and
//! the [`MaterializationDriver`] (the W54-2 production wiring that powers
//! the W31-F materialization scheduler over the plan authority — periodic
//! select→gate-drive passes per durable plan, Task admission consult
//! included), and serves two local endpoints side by side:
//!
//! - **authenticated entry** — [`authenticated_serve_one_control`] (the GUI's
//!   only wiring shape; every connection answers the ADR-0011
//!   challenge-response handshake);
//! - **plain entry** — `serve_one` + [`RecoverySystemControl::handle_for_ipc`]
//!   for the `system-control-cli` binary.
//!
//! The two endpoints are platform-dispatched exactly like the `nlos-ipc`
//! transport and [`crate::auth`] entries they consume: Unix-domain sockets
//! on Unix, named pipes on Windows (see [`crate::auth::EndpointListener`]
//! and the default-endpoint derivation on [`DaemonOptions`]). Only transport
//! acquisition (bind/accept) and the endpoint-path form differ per platform;
//! handshake and exchange wire bytes are identical on every platform.
//!
//! Both entries share one handler construction path with every
//! `with_*_source` inspector seam wired **and the executor arms powered**
//! (W59-1, the F1/Lane-1 assembly): one daemon-private composite operation
//! executor composes the three landed operation executors — kill through
//! [`crate::process_kill_executor::ProcessAuthorityKillExecutor`] over the
//! daemon's process authority + in-process
//! [`nlos_process::SupervisorPidRegistry`] + the platform kill adapter,
//! throttle through
//! [`crate::resource_throttle_executor::ResourceDemandThrottleExecutor`]
//! over the daemon's resource authority (W44-SC1 durable decision ledger),
//! reclaim through
//! [`crate::working_set_reclaim_executor::WorkingSetReclaimExecutor`] over
//! the real task-authority occupancy face — and
//! [`crate::application_lifecycle_executor::ApplicationAuthorityLifecycleExecutor`]
//! powers the disable/uninstall arms (uninstall through the W27-D
//! task-activity gate over the daemon's own task authority). The arms with
//! no implementation anywhere — pause/resume/cancel — refuse fail-closed
//! with a typed `NOT_FOUND` naming the unwired arm, exactly like the
//! [`crate::UnwiredOperationCommandExecutor`] they replaced.
//!
//! Honest registration-surface gap: the daemon owns the
//! [`nlos_process::SupervisorPidRegistry`] as an in-process, daemon-lifetime
//! instance, but no supervisor loop inside this daemon registers pid
//! mappings yet, so a kill against a never-registered target refuses typed
//! `NOT_FOUND` ("no supervisor os pid mapping is registered") — the honest
//! production behavior, not a fabricated signal. Hosts (and tests) seed
//! mappings through [`SystemControlDaemon::register_supervisor_pid`]; the
//! per-exchange platform-kill adapter snapshots the registry's current
//! pid map, mirroring the slice-k teardown usage.
//!
//! The `Process`/`Resource`/`Application` authorities are opened because the
//! daemon is the resident owner of the state root (and the worker drives the
//! resource half through its own `ResourceAuthority` handle, its semantic
//! half through its own `SemanticAuthority` handle); their
//! inspector adapters are composed **client-side** per the
//! `ControlReceipt::compose` contract, exactly like the CLI `--root` path —
//! and [`SystemControlDaemon::dispatch_command`] performs the isomorphic
//! daemon-side composition over the same authorities it owns, so the
//! in-process service-parity face carries the three composed inspections
//! without crossing a socket.
//!
//! Process/resource/application inspection therefore never crosses these
//! sockets as server-side state; the GET envelope still crosses for
//! authorization parity.

use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nlos_application::ApplicationAuthority;
use nlos_artifact::ArtifactStore;
use nlos_channel::{ChannelAuthority, ChannelAuthorityError};
use nlos_clock::{AuthorityClock, AuthorityClockError};
use nlos_commit_coordinator::{
    RecoveryWorkerConfig, RecoveryWorkerHealth, RecoveryWorkerStartError,
    TaskAuthorityCommitRecoveryWorker,
};
use nlos_identity::{
    BootstrapDecision, BootstrapPrincipalRequest, IdentityAuthority, IdentityAuthorityError,
    KeyPurpose,
};
use nlos_ipc::handshake::transport::ServerHandshakeContext;
#[cfg(unix)]
use nlos_ipc::unix::UnixListenerAdapter;
#[cfg(windows)]
use nlos_ipc::windows::NamedPipeListenerAdapter;
use nlos_ipc::{
    OutboundResponse, PeerAuthorizer, PeerIdentity, TransportConfig, handshake::HandshakeError,
    serve_one,
};
use nlos_plan::{PlanStoreError, SqlitePlanAuthority};
#[cfg(unix)]
use nlos_process::PosixPlatformKillAdapter;
#[cfg(windows)]
use nlos_process::WindowsPlatformKillAdapter;
use nlos_process::{
    ProcessAuthority, ProcessAuthorityError, RegisterSupervisorPidRequest, SupervisorPidDecision,
    SupervisorPidRegistry, SupervisorPidRegistryError,
};
use nlos_resource::{ResourceAuthority, ResourceAuthorityError};
use nlos_runtime::RuntimeError;
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig};
use nlos_schema::sabi::v1::{
    ControlCommand as SabiWireCommand, ExchangeResponse, GetSystemControlRequest, SabiErrorCode,
    SabiFailure, SabiRequestContext,
};
use nlos_semantic::{SemanticAuthority, SemanticAuthorityError};
use nlos_store::{SqliteOperationStore, StoreError};
use nlos_task::{SqliteTaskAuthority, TASK_PROFILE_10K, TaskStoreError};
use nlos_topic::{TopicAuthority, TopicAuthorityError};
use nlos_types::IdempotencyKey;
use sha2::{Digest, Sha256};

use crate::application_inspector::ApplicationAuthorityInspector;
use crate::application_lifecycle_executor::ApplicationAuthorityLifecycleExecutor;
use crate::auth::EndpointListener;
use crate::auth::authenticated_serve_one_control;
use crate::control::{
    CONTROL_CAPABILITY_GENERATION, CONTROL_CAPABILITY_SLOT, ControlCommand, ControlError,
    ControlReceipt,
};
use crate::fiber_inspector::TokioExecutionFiberSource;
use crate::materialization_driver::{
    MaterializationDriver, MaterializationDriverConfig, MaterializationDriverHealth,
    MaterializationDriverStartError,
};
use crate::operation_inspector::OperationStoreSource;
use crate::plan_inspector::PlanAuthorityTaskNodeSource;
use crate::process_inspector::ProcessAuthorityInspector;
use crate::process_kill_executor::ProcessAuthorityKillExecutor;
use crate::resource_inspector::ResourceAuthorityInspector;
use crate::resource_throttle_executor::ResourceDemandThrottleExecutor;
use crate::topic_inspector::TopicAuthoritySource;
use crate::working_set_reclaim_executor::{
    TaskAuthorityWorkingSetOccupancy, WorkingSetReclaimExecutor,
};
use crate::{
    OperationCommandExecutor, RecoveryHealthSource, RecoverySystemControl, SystemControlAuthorizer,
};

/// Key validity ceiling for daemon-bootstrapped principals: 2100-01-01. Not
/// a secret; only an upper bound, mirroring the dev-fixture window.
const BOOTSTRAP_KEY_VALID_UNTIL_MS: u64 = 4_102_444_800_000;
/// Handshake nonce registry capacity for the authenticated endpoint.
const HANDSHAKE_NONCE_CAPACITY: usize = 64;
/// Named-pipe instance budget for one daemon endpoint on Windows (the
/// `bind`/`accept` contract requires at least two instances: one listening,
/// one connected). The service loops serve one exchange per round, so a
/// small backlog lets clients queue while one exchange is in flight.
#[cfg(windows)]
const DAEMON_PIPE_INSTANCES: usize = 4;

// Default endpoint-path derivation, centralized in this one place per OS
// form:
//
// - Unix — owner-only filesystem sockets inside the state root:
//   `<root>/system-control-auth.sock` and `<root>/system-control-plain.sock`
//   (byte-for-byte the historical defaults);
// - Windows — named pipes in the machine-global pipe namespace:
//   `\\.\pipe\llmos-system-control-{auth|plain}-<root-digest>`. The
//   namespace is shared by every process on the machine, so a 16-hex digest
//   of the state root's OS path bytes keeps distinct roots on distinct pipe
//   names; two daemons on the same root (same spelling) still collide
//   fail-closed at bind (the named-pipe first-instance flag), mirroring the
//   Unix stale-path posture without a filesystem artifact to unlink.
//
// The daemon never mixes forms: an explicit
// `DaemonOptions::with_auth_socket`/`with_plain_socket` override is used
// verbatim and must already carry the intended platform form, because the
// authenticated channel binding hashes exactly these path bytes on both
// ends.
//
/// Default authenticated-endpoint path, per-OS form (see the centralized
/// derivation rules above): `<root>/system-control-auth.sock` on Unix.
#[cfg(unix)]
fn auth_endpoint_default(root: &Path) -> PathBuf {
    root.join("system-control-auth.sock")
}

/// Default authenticated-endpoint path, per-OS form (see the centralized
/// derivation rules above): `\\.\pipe\llmos-system-control-auth-<digest>` on
/// Windows.
#[cfg(windows)]
fn auth_endpoint_default(root: &Path) -> PathBuf {
    default_pipe_endpoint("auth", root)
}

/// Default plain-endpoint path, per-OS form (see the centralized derivation
/// rules above): `<root>/system-control-plain.sock` on Unix.
#[cfg(unix)]
fn plain_endpoint_default(root: &Path) -> PathBuf {
    root.join("system-control-plain.sock")
}

/// Default plain-endpoint path, per-OS form (see the centralized derivation
/// rules above): `\\.\pipe\llmos-system-control-plain-<digest>` on Windows.
#[cfg(windows)]
fn plain_endpoint_default(root: &Path) -> PathBuf {
    default_pipe_endpoint("plain", root)
}

/// Windows default form: `\\.\pipe\llmos-system-control-<role>-<root-digest>`
/// with the digest domain-separated and truncated to 16 hex characters
/// (well inside the kernel's 256-character pipe-name bound).
#[cfg(windows)]
fn default_pipe_endpoint(role: &str, root: &Path) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/system-control-daemon/pipe-name/v1");
    hasher.update(root.as_os_str().as_encoded_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    PathBuf::from(format!(
        r"\\.\pipe\llmos-system-control-{role}-{}",
        hex(&digest[..8])
    ))
}

/// Typed assembly failure. No durable state is changed solely by reporting
/// one of these; every authority the daemon already opened stays valid for
/// the caller to drop.
#[derive(Debug)]
pub enum DaemonError {
    Io(std::io::Error),
    Identity(IdentityAuthorityError),
    Clock(AuthorityClockError),
    Task(TaskStoreError),
    Artifact(nlos_artifact::ArtifactError),
    Semantic(SemanticAuthorityError),
    Process(ProcessAuthorityError),
    Resource(ResourceAuthorityError),
    Application(nlos_application::ApplicationAuthorityError),
    Plan(PlanStoreError),
    Channel(ChannelAuthorityError),
    Topic(TopicAuthorityError),
    Store(StoreError),
    Runtime(RuntimeError),
    Ipc(nlos_ipc::IpcError),
    Handshake(HandshakeError),
    Worker(RecoveryWorkerStartError),
    MaterializationDriver(MaterializationDriverStartError),
    /// The `--identity-key-file` content violates the 64-hex Ed25519 seed
    /// contract shared with the desktop client.
    KeyFile(&'static str),
}

impl fmt::Display for DaemonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "daemon io: {error}"),
            Self::Identity(error) => write!(formatter, "identity authority: {error}"),
            Self::Clock(error) => write!(formatter, "authority clock: {error}"),
            Self::Task(error) => write!(formatter, "task authority: {error}"),
            Self::Artifact(error) => write!(formatter, "artifact store: {error}"),
            Self::Semantic(error) => write!(formatter, "semantic authority: {error}"),
            Self::Process(error) => write!(formatter, "process authority: {error}"),
            Self::Resource(error) => write!(formatter, "resource authority: {error}"),
            Self::Application(error) => write!(formatter, "application authority: {error}"),
            Self::Plan(error) => write!(formatter, "plan authority: {error}"),
            Self::Channel(error) => write!(formatter, "channel authority: {error}"),
            Self::Topic(error) => write!(formatter, "topic authority: {error}"),
            Self::Store(error) => write!(formatter, "operation store: {error}"),
            Self::Runtime(error) => write!(formatter, "runtime adapter: {error}"),
            Self::Ipc(error) => write!(formatter, "ipc endpoint: {error}"),
            Self::Handshake(error) => write!(formatter, "handshake context: {error}"),
            Self::Worker(error) => write!(formatter, "recovery worker: {error}"),
            Self::MaterializationDriver(error) => {
                write!(formatter, "materialization driver: {error}")
            }
            Self::KeyFile(reason) => write!(formatter, "identity key file: {reason}"),
        }
    }
}

impl Error for DaemonError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Identity(error) => Some(error),
            Self::Clock(error) => Some(error),
            Self::Task(error) => Some(error),
            Self::Artifact(error) => Some(error),
            Self::Semantic(error) => Some(error),
            Self::Process(error) => Some(error),
            Self::Resource(error) => Some(error),
            Self::Application(error) => Some(error),
            Self::Plan(error) => Some(error),
            Self::Channel(error) => Some(error),
            Self::Topic(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Runtime(error) => Some(error),
            Self::Ipc(error) => Some(error),
            Self::Handshake(error) => Some(error),
            Self::Worker(error) => Some(error),
            Self::MaterializationDriver(error) => Some(error),
            Self::KeyFile(_) => None,
        }
    }
}

/// Daemon startup options.
pub struct DaemonOptions {
    /// State root every authority is opened under.
    pub root: PathBuf,
    /// Authenticated endpoint path; the per-OS default is derived from the
    /// root (see the centralized derivation rules above the
    /// `*_endpoint_default` helpers).
    pub auth_socket: Option<PathBuf>,
    /// Plain endpoint path; the per-OS default is derived from the root
    /// (see the centralized derivation rules above the
    /// `*_endpoint_default` helpers).
    pub plain_socket: Option<PathBuf>,
    /// Optional 64-hex Ed25519 seed file (the desktop client's 0600 key-file
    /// format). When present the daemon bootstraps (or idempotently replays)
    /// the matching principal in the identity authority so a client holding
    /// the same seed can authenticate.
    pub identity_key_file: Option<PathBuf>,
    /// Recovery worker tuning; the first scan runs immediately on start.
    pub worker_config: RecoveryWorkerConfig,
    /// Materialization driver tuning (window, cadence, backoff, fault
    /// threshold); the first scheduler pass runs immediately on start.
    pub driver_config: MaterializationDriverConfig,
}

impl DaemonOptions {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            auth_socket: None,
            plain_socket: None,
            identity_key_file: None,
            worker_config: RecoveryWorkerConfig::default(),
            driver_config: MaterializationDriverConfig::default(),
        }
    }

    /// Overrides the authenticated endpoint path (Unix socket path on Unix,
    /// `\\.\pipe\...` pipe name on Windows).
    #[must_use]
    pub fn with_auth_socket(mut self, path: impl Into<PathBuf>) -> Self {
        self.auth_socket = Some(path.into());
        self
    }

    /// Overrides the plain endpoint path (Unix socket path on Unix,
    /// `\\.\pipe\...` pipe name on Windows).
    #[must_use]
    pub fn with_plain_socket(mut self, path: impl Into<PathBuf>) -> Self {
        self.plain_socket = Some(path.into());
        self
    }

    /// Requests an Ed25519 seed bootstrap for the authenticated entry.
    #[must_use]
    pub fn with_identity_key_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.identity_key_file = Some(path.into());
        self
    }

    /// Overrides the recovery worker tuning.
    #[must_use]
    pub const fn with_worker_config(mut self, config: RecoveryWorkerConfig) -> Self {
        self.worker_config = config;
        self
    }

    /// Overrides the materialization driver tuning.
    #[must_use]
    pub const fn with_driver_config(mut self, config: MaterializationDriverConfig) -> Self {
        self.driver_config = config;
        self
    }

    fn auth_socket_path(&self) -> PathBuf {
        self.auth_socket
            .clone()
            .unwrap_or_else(|| auth_endpoint_default(&self.root))
    }

    fn plain_socket_path(&self) -> PathBuf {
        self.plain_socket
            .clone()
            .unwrap_or_else(|| plain_endpoint_default(&self.root))
    }
}

/// The worker handle shared between the handler (read-only health face) and
/// the shutdown path (idempotent stop/join).
pub struct SharedRecoveryWorker(Mutex<TaskAuthorityCommitRecoveryWorker>);

impl SharedRecoveryWorker {
    fn lock(&self) -> std::sync::MutexGuard<'_, TaskAuthorityCommitRecoveryWorker> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Current worker health (the [`RecoveryHealthSource`] face is the same
    /// read, typed for the handler).
    #[must_use]
    pub fn health(&self) -> RecoveryWorkerHealth {
        self.lock().health()
    }

    /// Requests shutdown and joins the dedicated thread. Repeated calls are
    /// harmless; `Drop` of the underlying worker runs the same stop.
    pub fn stop(&self) {
        self.lock().stop();
    }
}

impl RecoveryHealthSource for SharedRecoveryWorker {
    fn recovery_health(&self) -> RecoveryWorkerHealth {
        self.health()
    }
}

/// The materialization-driver handle shared between the handler
/// (read-only health face) and the shutdown path (idempotent stop/join),
/// mirroring [`SharedRecoveryWorker`].
pub struct SharedMaterializationDriver(Mutex<MaterializationDriver>);

impl SharedMaterializationDriver {
    fn lock(&self) -> std::sync::MutexGuard<'_, MaterializationDriver> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Current driver health (state, pass counters, window, seats,
    /// failure posture).
    #[must_use]
    pub fn health(&self) -> MaterializationDriverHealth {
        self.lock().health()
    }

    /// Requests shutdown and joins the dedicated thread. Repeated calls
    /// are harmless; `Drop` of the underlying driver runs the same stop.
    pub fn stop(&self) {
        self.lock().stop();
    }
}

/// Capability policy shared with the dev fixture and test harnesses: only
/// the control-plane prefix's fixed handle slot authorizes an exchange.
pub struct ControlCapabilityPolicy;

impl SystemControlAuthorizer for ControlCapabilityPolicy {
    fn authorize_get(
        &self,
        context: &SabiRequestContext,
        _: &GetSystemControlRequest,
    ) -> Result<(), &'static str> {
        authorize_capability(context)
    }

    fn authorize_submit(
        &self,
        context: &SabiRequestContext,
        _: &SabiWireCommand,
    ) -> Result<(), &'static str> {
        authorize_capability(context)
    }
}

fn authorize_capability(context: &SabiRequestContext) -> Result<(), &'static str> {
    let expected = nlos_schema::sabi::v1::CapabilityHandle {
        slot: CONTROL_CAPABILITY_SLOT,
        generation: CONTROL_CAPABILITY_GENERATION,
    };
    if context.capability_handles.as_slice() == [expected] {
        Ok(())
    } else {
        Err("missing recovery operations capability")
    }
}

/// Peer gate for both endpoints: the authenticated endpoint has already
/// verified the connection through the ADR-0011 handshake, and the plain
/// endpoint stays inside the local trust domain (owner-only 0600 sockets on
/// Unix, the machine-local named-pipe namespace on Windows), so the
/// in-transport peer gate admits and the capability policy remains the
/// boundary.
struct AllowPeer;

impl PeerAuthorizer for AllowPeer {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

/// The platform kill adapter the daemon injects: the real POSIX
/// `kill(SIGTERM)` adapter on Unix, the real `TerminateProcess`-backed
/// adapter on Windows (the daemon module itself is Unix/Windows-only —
/// it has no other `bind_socket` path).
#[cfg(unix)]
type DaemonPlatformKillAdapter = PosixPlatformKillAdapter;
#[cfg(windows)]
type DaemonPlatformKillAdapter = WindowsPlatformKillAdapter;

/// Builds one platform kill adapter over the supervisor registry's
/// **current** generation-keyed pid-map snapshot (W59-2 fenced shape: the
/// map keys carry `(ProcessId, Generation)` so a stale-generation kill can
/// never resolve to a live pid even if the OS recycled it). The daemon
/// builds it per handler round, so a registration made between two
/// exchanges is observed by the next round's kill arm.
fn platform_kill_adapter(registry: &SupervisorPidRegistry) -> DaemonPlatformKillAdapter {
    #[cfg(unix)]
    {
        PosixPlatformKillAdapter::with_generation_pid_map(registry.generation_pid_map())
    }
    #[cfg(windows)]
    {
        WindowsPlatformKillAdapter::with_generation_pid_map(registry.generation_pid_map())
    }
}

/// Composite operation-level executor of the resident daemon (W59-1): one
/// [`OperationCommandExecutor`] seam entry delegating each powered arm to
/// the landed authority-backed executor that owns it — kill to
/// [`ProcessAuthorityKillExecutor`] over the daemon's process authority +
/// supervisor registry + platform adapter, throttle to
/// [`ResourceDemandThrottleExecutor`] over the daemon's resource authority,
/// reclaim to [`WorkingSetReclaimExecutor`] over the real task-authority
/// occupancy face. The arms no implementation exists for anywhere —
/// pause/resume/cancel — keep the [`crate::UnwiredOperationCommandExecutor`]
/// posture: a typed fail-closed `NOT_FOUND` naming the arm (registered gap;
/// the wire surface and handler semantics stay owned by the shared submit
/// path).
///
/// The struct owns the per-exchange platform kill adapter (a pid-map
/// snapshot of the registry, mirroring the slice-k teardown usage) and
/// borrows only daemon-lifetime authorities, so one value serves a whole
/// exchange; each arm method builds its concrete executor for the call
/// itself.
struct DaemonOperationExecutor<'a> {
    process: &'a ProcessAuthority,
    supervisor: &'a SupervisorPidRegistry,
    resource: &'a ResourceAuthority,
    tasks: &'a SqliteTaskAuthority,
    kill_adapter: DaemonPlatformKillAdapter,
}

impl<'a> DaemonOperationExecutor<'a> {
    /// Snapshots the registry's current pid map into the platform adapter
    /// at construction time, so every exchange observes the supervisor
    /// registrations made before it started.
    fn new(daemon: &'a SystemControlDaemon) -> Self {
        Self {
            process: daemon.process.as_ref(),
            supervisor: &daemon.supervisor_registry,
            resource: daemon.resource.as_ref(),
            tasks: daemon.tasks.as_ref(),
            kill_adapter: platform_kill_adapter(&daemon.supervisor_registry),
        }
    }
}

impl OperationCommandExecutor for DaemonOperationExecutor<'_> {
    fn kill_operation(
        &self,
        request: crate::OperationControlRequest,
    ) -> Result<nlos_types::ReceiptId, SabiFailure> {
        ProcessAuthorityKillExecutor::new(self.process, self.supervisor, &self.kill_adapter)
            .kill_operation(request)
    }

    fn throttle_operation(
        &self,
        request: crate::OperationControlRequest,
        throttle_percent: u64,
    ) -> Result<nlos_types::ReceiptId, SabiFailure> {
        ResourceDemandThrottleExecutor::new(self.resource)
            .throttle_operation(request, throttle_percent)
    }

    fn reclaim_operation(
        &self,
        request: crate::OperationControlRequest,
    ) -> Result<nlos_types::ReceiptId, SabiFailure> {
        WorkingSetReclaimExecutor::new(
            // The tier the daemon's task authority was opened with
            // (`SqliteTaskAuthority::open` defaults to `TASK_PROFILE_10K`);
            // the occupancy observation below is the same authority's own
            // issued-permit count, so the two cannot disagree.
            TASK_PROFILE_10K,
            TaskAuthorityWorkingSetOccupancy::new(self.tasks),
        )
        .reclaim_operation(request)
    }

    fn pause_operation(
        &self,
        _: crate::OperationControlRequest,
    ) -> Result<nlos_types::ReceiptId, SabiFailure> {
        Err(arm_unwired("pause"))
    }

    fn resume_operation(
        &self,
        _: crate::OperationControlRequest,
    ) -> Result<nlos_types::ReceiptId, SabiFailure> {
        Err(arm_unwired("resume"))
    }

    fn cancel_operation(
        &self,
        _: crate::OperationControlRequest,
    ) -> Result<nlos_types::ReceiptId, SabiFailure> {
        Err(arm_unwired("cancel"))
    }
}

/// Typed fail-closed refusal of one arm with no implementation anywhere;
/// same code/retry posture as [`crate::UnwiredOperationCommandExecutor`],
/// naming the arm for the operator.
fn arm_unwired(arm: &'static str) -> SabiFailure {
    SabiFailure {
        code: SabiErrorCode::NotFound.into(),
        retry: nlos_schema::sabi::v1::RetryDirective::DoNotRetry.into(),
        safe_message: format!("the {arm} arm is not wired in the system-control daemon"),
    }
}

/// The assembled daemon: every authority handle plus the running worker.
/// All fields are read-only handles; nothing here is mutated after
/// [`assemble`].
pub struct SystemControlDaemon {
    /// State root the authorities were opened under.
    pub root: PathBuf,
    /// Bound authenticated endpoint path (already printed in the READY line).
    pub auth_socket_path: PathBuf,
    /// Bound plain endpoint path (already printed in the READY line).
    pub plain_socket_path: PathBuf,
    /// Principal id bootstrapped from `--identity-key-file`, if requested
    /// (32 hex).
    pub bootstrapped_principal_hex: Option<String>,
    /// ADR-0011 verifier for the authenticated endpoint.
    pub identity: Arc<IdentityAuthority>,
    /// Durable wall/tick source for handshake and command time.
    pub clock: Arc<AuthorityClock>,
    /// Recovery ledger and command authority (the handler core).
    pub tasks: Arc<SqliteTaskAuthority>,
    /// Artifact store driven by the recovery worker.
    pub artifacts: Arc<ArtifactStore>,
    /// Semantic authority (worker semantic half; resident owner of the
    /// state root, mirroring the resource half).
    pub semantic: Arc<SemanticAuthority>,
    /// Process authority (resident owner; client-side inspector surface).
    pub process: Arc<ProcessAuthority>,
    /// Resource authority (worker resource half + client-side inspector).
    pub resource: Arc<ResourceAuthority>,
    /// Application authority (resident owner; client-side inspector surface).
    pub application: Arc<ApplicationAuthority>,
    /// Plan authority behind the `TaskNode` inspect seam and the
    /// materialization driver's scheduler passes.
    pub plans: Arc<SqlitePlanAuthority>,
    /// Channel authority the topic authority is bound to.
    pub channel: Arc<ChannelAuthority>,
    /// Topic authority behind the `Topic` inspect seam.
    pub topics: TopicAuthority,
    /// Operation store behind the `Operation` inspect seam.
    pub operations: SqliteOperationStore,
    /// Live runtime adapter behind the `ExecutionFiber` inspect seam.
    pub runtime: TokioRuntimeAdapter,
    /// In-process supervisor pid registry behind the kill arm's platform
    /// adapter (daemon-lifetime memory instance; see
    /// [`Self::register_supervisor_pid`] for the honest registration-surface
    /// gap).
    supervisor_registry: SupervisorPidRegistry,
    /// Handshake nonce registry bound to the authenticated endpoint.
    pub handshake: Arc<ServerHandshakeContext>,
    /// One-time handshake nonce source, seeded from the OS.
    random: RandomSource,
    /// Running recovery worker (health face + idempotent stop).
    worker: Arc<SharedRecoveryWorker>,
    /// Running materialization driver (health face + idempotent stop).
    materialization: Arc<SharedMaterializationDriver>,
}

impl SystemControlDaemon {
    /// Current recovery worker health.
    #[must_use]
    pub fn recovery_health(&self) -> RecoveryWorkerHealth {
        self.worker.health()
    }

    /// Stops the recovery worker (idempotent stop + join).
    pub fn stop_worker(&self) {
        self.worker.stop();
    }

    /// Current materialization driver health (state, pass counters,
    /// window, seats in use, failure posture).
    #[must_use]
    pub fn materialization_health(&self) -> MaterializationDriverHealth {
        self.materialization.health()
    }

    /// Stops the materialization driver (idempotent stop + join).
    pub fn stop_materialization(&self) {
        self.materialization.stop();
    }

    /// Registers one supervisor-observed OS pid mapping for an
    /// authority-assigned `ProcessId` in the daemon's in-process registry
    /// (the registry the kill arm's platform adapter snapshots per
    /// handler round).
    ///
    /// Honest registration-surface gap (W59-1): the daemon runs no
    /// supervisor loop today, so nothing inside this process registers
    /// mappings in production — a kill against a never-registered target
    /// refuses typed `NOT_FOUND` ("no supervisor os pid mapping is
    /// registered for the process") before the process authority is
    /// driven. This face exists for the host supervisor lane that will
    /// own registration, and for assemblies (tests, embeddings) that seed
    /// real child mappings.
    ///
    /// # Errors
    ///
    /// Returns [`SupervisorPidRegistryError`] on a stale generation, a
    /// same-generation OS pid rebind, or a poisoned lock; rejections have
    /// zero side effect.
    pub fn register_supervisor_pid(
        &self,
        request: RegisterSupervisorPidRequest,
    ) -> Result<SupervisorPidDecision, SupervisorPidRegistryError> {
        self.supervisor_registry.register(request)
    }

    /// Builds one exchange's executor stack (W59-1): the composite
    /// operation executor (kill/throttle/reclaim powered; the composite
    /// snapshots the registry pid map here, so registrations made before
    /// this exchange are observed) and the application lifecycle executor.
    /// The four layer inspect sources and the handler itself are built at
    /// the call site — their borrows must stay in the caller's frame.
    fn executor_stack(
        &self,
    ) -> (
        DaemonOperationExecutor<'_>,
        ApplicationAuthorityLifecycleExecutor<'_>,
    ) {
        (
            DaemonOperationExecutor::new(self),
            ApplicationAuthorityLifecycleExecutor::new(
                self.application.as_ref(),
                self.tasks.as_ref(),
            ),
        )
    }

    /// Dispatches one [`ControlCommand`] in-process through the daemon's own
    /// handler (service parity face for tests and health probes) with the
    /// executor arms powered. The three client-composed inspector slots
    /// (`Process`/`Resource`/`Application`) are composed daemon-side over
    /// the very authorities this daemon owns — the same
    /// `ControlReceipt::compose` contract the CLI `--root` client applies
    /// client-side, here isomorphic because dispatcher and service share
    /// one process.
    ///
    /// # Errors
    ///
    /// Returns [`ControlError`] when envelope compilation or receipt
    /// projection fails; handler rejections surface as typed receipt
    /// failures instead.
    pub fn dispatch_command(
        &self,
        command: &ControlCommand,
        now_monotonic_ns: u64,
        now_wall_ms: i64,
    ) -> Result<ControlReceipt, ControlError> {
        let plans = PlanAuthorityTaskNodeSource::new(&self.plans);
        let fibers = TokioExecutionFiberSource::new(&self.runtime);
        let topics = TopicAuthoritySource::new(&self.topics);
        let operations = OperationStoreSource::new(&self.operations);
        let (operation_arms, applications) = self.executor_stack();
        let control = RecoverySystemControl::new(
            self.tasks.as_ref(),
            self.worker.as_ref(),
            &ControlCapabilityPolicy,
        )
        .with_task_node_source(&plans)
        .with_execution_fiber_source(&fibers)
        .with_topic_source(&topics)
        .with_operation_source(&operations)
        .with_operation_executor(&operation_arms)
        .with_application_executor(&applications);
        let process = ProcessAuthorityInspector::new(self.process.as_ref());
        let resource = ResourceAuthorityInspector::new(self.resource.as_ref());
        let application = ApplicationAuthorityInspector::new(self.application.as_ref());
        crate::control::dispatch_in_process(
            &control,
            command,
            now_monotonic_ns,
            now_wall_ms,
            Some(&process),
            Some(&resource),
            Some(&application),
        )
    }
}

/// Bound listeners for the two endpoints, handed to the service loops. The
/// listener type is the platform [`EndpointListener`] (Unix-domain socket
/// on Unix, named pipe on Windows), so the loops stay platform-neutral.
pub struct DaemonEndpoints {
    pub listener_authenticated: EndpointListener,
    pub listener_plain: EndpointListener,
}

/// Assembles the daemon: opens every authority under `options.root`, starts
/// the recovery worker with all three recovery domains live (the artifact,
/// semantic, and resource halves each scan and converge their due plans),
/// optionally bootstraps the `--identity-key-file` principal, and binds both
/// endpoints.
///
/// # Errors
///
/// Fails closed with [`DaemonError`] on the first authority, worker, or
/// bind failure; nothing is served.
pub fn assemble(
    options: DaemonOptions,
    runtime_handle: tokio::runtime::Handle,
) -> Result<(Arc<SystemControlDaemon>, DaemonEndpoints), DaemonError> {
    let random = RandomSource::from_os()?;
    fs::create_dir_all(&options.root).map_err(DaemonError::Io)?;
    let auth_socket_path = options.auth_socket_path();
    let plain_socket_path = options.plain_socket_path();

    let identity =
        IdentityAuthority::open(options.root.join("identity")).map_err(DaemonError::Identity)?;
    let bootstrapped_principal_hex = match &options.identity_key_file {
        Some(key_file) => {
            let principal = bootstrap_principal(&identity, key_file)?;
            Some(hex(principal.as_bytes()))
        }
        None => None,
    };
    let clock = AuthorityClock::open(options.root.join("clock")).map_err(DaemonError::Clock)?;
    let clock = Arc::new(clock);
    let tasks = Arc::new(
        SqliteTaskAuthority::open(options.root.join("tasks.sqlite3")).map_err(DaemonError::Task)?,
    );
    let artifacts = Arc::new(
        ArtifactStore::open(options.root.join("artifacts")).map_err(DaemonError::Artifact)?,
    );
    let semantic = Arc::new(
        SemanticAuthority::open(options.root.join("semantic")).map_err(DaemonError::Semantic)?,
    );
    let process = Arc::new(
        ProcessAuthority::open(options.root.join("process")).map_err(DaemonError::Process)?,
    );
    let resource = Arc::new(
        ResourceAuthority::open(options.root.join("resource")).map_err(DaemonError::Resource)?,
    );
    let application = Arc::new(
        ApplicationAuthority::open(options.root.join("application"))
            .map_err(DaemonError::Application)?,
    );
    let plans = Arc::new(
        SqlitePlanAuthority::open(options.root.join("plans.sqlite3")).map_err(DaemonError::Plan)?,
    );
    let channel = Arc::new(
        ChannelAuthority::open(options.root.join("channel")).map_err(DaemonError::Channel)?,
    );
    let topics = TopicAuthority::open(options.root.join("topics"), Arc::clone(&channel))
        .map_err(DaemonError::Topic)?;
    let operations = SqliteOperationStore::open(options.root.join("operations.sqlite3"))
        .map_err(DaemonError::Store)?;
    let runtime = TokioRuntimeAdapter::new(runtime_handle, TokioRuntimeConfig::default())
        .map_err(DaemonError::Runtime)?;
    // The worker drives all three recovery domains: the artifact half
    // through `artifacts`, the semantic half through `semantic`, and the
    // resource half through `resource`, so due plans in every domain are
    // scanned and converged.
    let worker = TaskAuthorityCommitRecoveryWorker::start_with_semantic_and_resource_authorities(
        Arc::clone(&tasks),
        Arc::clone(&artifacts),
        Some(Arc::clone(&semantic)),
        Some(Arc::clone(&resource)),
        options.worker_config,
    )
    .map_err(DaemonError::Worker)?;
    // The materialization driver powers the W31-F scheduler over the
    // same plan authority the inspect seam reads and the same task
    // authority the worker recovers through: periodic passes drive the
    // plan→materialization→task-admission chain (select → gate drive per
    // plan), with the Task consult wired in (see the module docs for the
    // deliberately-unwired ecosystem selector). The driver shares this
    // daemon's clock so its durable gate-round markers are stamped by the
    // AuthorityClock wall domain (F10), not the bare system clock.
    let materialization_driver = MaterializationDriver::start(
        Arc::clone(&plans),
        Arc::clone(&tasks),
        Arc::clone(&clock),
        options.driver_config,
    )
    .map_err(DaemonError::MaterializationDriver)?;
    let handshake = Arc::new(
        ServerHandshakeContext::new(&auth_socket_path, HANDSHAKE_NONCE_CAPACITY)
            .map_err(DaemonError::Handshake)?,
    );
    let listener_authenticated = bind_socket(&auth_socket_path)?;
    let listener_plain = bind_socket(&plain_socket_path)?;

    let daemon = Arc::new(SystemControlDaemon {
        root: options.root,
        auth_socket_path,
        plain_socket_path,
        bootstrapped_principal_hex,
        identity: Arc::new(identity),
        clock,
        tasks,
        artifacts,
        semantic,
        process,
        resource,
        application,
        plans,
        channel,
        topics,
        operations,
        runtime,
        supervisor_registry: SupervisorPidRegistry::new(),
        handshake,
        random,
        worker: Arc::new(SharedRecoveryWorker(Mutex::new(worker))),
        materialization: Arc::new(SharedMaterializationDriver(Mutex::new(
            materialization_driver,
        ))),
    });
    Ok((
        daemon,
        DaemonEndpoints {
            listener_authenticated,
            listener_plain,
        },
    ))
}

/// Serves the authenticated endpoint until `stop` is observed: one
/// ADR-0011-handshake-verified connection per round, one control exchange
/// per connection. Idle accept windows are normal rounds; transport errors
/// back off briefly instead of retiring the endpoint.
///
/// # Panics
///
/// Never panics by construction; every failure path is a logged round.
pub async fn serve_authenticated_endpoint(
    daemon: Arc<SystemControlDaemon>,
    mut listener: EndpointListener,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        let plans = PlanAuthorityTaskNodeSource::new(&daemon.plans);
        let fibers = TokioExecutionFiberSource::new(&daemon.runtime);
        let topics = TopicAuthoritySource::new(&daemon.topics);
        let operations = OperationStoreSource::new(&daemon.operations);
        let (operation_arms, applications) = daemon.executor_stack();
        let control = RecoverySystemControl::new(
            daemon.tasks.as_ref(),
            daemon.worker.as_ref(),
            &ControlCapabilityPolicy,
        )
        .with_task_node_source(&plans)
        .with_execution_fiber_source(&fibers)
        .with_topic_source(&topics)
        .with_operation_source(&operations)
        .with_operation_executor(&operation_arms)
        .with_application_executor(&applications);
        let outcome = authenticated_serve_one_control(
            &mut listener,
            TransportConfig::default(),
            &control,
            daemon.identity.as_ref(),
            daemon.clock.as_ref(),
            daemon.handshake.as_ref(),
            &AllowPeer,
            monotonic_now_ns(),
            || daemon.random.bytes32(),
        )
        .await;
        if let Err(error) = outcome {
            // Idle accept timeouts and peer handshake failures are normal
            // rounds; anything else backs off so the loop cannot hot-spin.
            eprintln!("system-control-daemon: authenticated accept/handshake: {error}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Serves the plain endpoint until `stop` is observed: one connection per
/// accept round, one `serve_one` exchange per connection against the shared
/// handler. One bad exchange never takes the endpoint down.
///
/// Command time semantics match the authenticated entry (F10/W61-A): the
/// exchange's wall time is the daemon `AuthorityClock`'s durable wall
/// reading issued (or durably replayed) for the request's §25.3 correlation
/// id through [`crate::auth::command_wall_key`] — the shared
/// [`crate::auth::serve_validated`] projection — so durable mutation
/// records (`acknowledged_at_ms`, `resumed_at_ms`, ...) are never stamped
/// from the bare system clock and a retried command re-reads its original
/// reading on either endpoint.
///
/// # Panics
///
/// Never panics by construction; every failure path is a logged round.
pub async fn serve_plain_endpoint(
    daemon: Arc<SystemControlDaemon>,
    mut listener: EndpointListener,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        let (stream, peer) = match accept_plain(&mut listener).await {
            Ok(connection) => connection,
            Err(nlos_ipc::IpcError::Timeout(nlos_ipc::IoOperation::Accept)) => continue,
            Err(error) => {
                eprintln!("system-control-daemon: plain accept: {error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let round = Arc::clone(&daemon);
        let _ = serve_one(
            stream,
            TransportConfig::default(),
            peer,
            &AllowPeer,
            move |validated| {
                let daemon = round;
                async move {
                    let plans = PlanAuthorityTaskNodeSource::new(&daemon.plans);
                    let fibers = TokioExecutionFiberSource::new(&daemon.runtime);
                    let topics = TopicAuthoritySource::new(&daemon.topics);
                    let operations = OperationStoreSource::new(&daemon.operations);
                    let (operation_arms, applications) = daemon.executor_stack();
                    let control = RecoverySystemControl::new(
                        daemon.tasks.as_ref(),
                        daemon.worker.as_ref(),
                        &ControlCapabilityPolicy,
                    )
                    .with_task_node_source(&plans)
                    .with_execution_fiber_source(&fibers)
                    .with_topic_source(&topics)
                    .with_operation_source(&operations)
                    .with_operation_executor(&operation_arms)
                    .with_application_executor(&applications);
                    let response = crate::auth::serve_validated(
                        &control,
                        daemon.clock.as_ref(),
                        monotonic_now_ns(),
                        validated.envelope(),
                    );
                    Ok(OutboundResponse::Typed(ExchangeResponse {
                        envelope: Some(response),
                    }))
                }
            },
        )
        .await;
    }
}

/// Platform dispatch for one plain-endpoint accept, mirroring the dual-path
/// [`nlos_ipc::handshake::transport`] accept halves: the Unix-domain socket
/// accept on Unix, the named-pipe accept on Windows (exclusive borrow — the
/// listener swaps in its next listening instance). Both wrappers hand the
/// platform stream to the same platform-neutral `serve_one` core, so the
/// exchange bytes and failure order are identical on every platform.
#[cfg(unix)]
async fn accept_plain(
    listener: &mut EndpointListener,
) -> Result<(tokio::net::UnixStream, PeerIdentity), nlos_ipc::IpcError> {
    listener.accept(TransportConfig::default()).await
}

/// Platform dispatch for one plain-endpoint accept, mirroring the dual-path
/// [`nlos_ipc::handshake::transport`] accept halves: the Unix-domain socket
/// accept on Unix, the named-pipe accept on Windows (exclusive borrow — the
/// listener swaps in its next listening instance). Both wrappers hand the
/// platform stream to the same platform-neutral `serve_one` core, so the
/// exchange bytes and failure order are identical on every platform.
#[cfg(windows)]
async fn accept_plain(
    listener: &mut EndpointListener,
) -> Result<
    (
        tokio::net::windows::named_pipe::NamedPipeServer,
        PeerIdentity,
    ),
    nlos_ipc::IpcError,
> {
    listener.accept(TransportConfig::default()).await
}

/// Removes a stale socket path (best effort — a live daemon keeps serving
/// its already-bound inode) and binds a fresh owner-only endpoint.
///
/// # Errors
///
/// Returns [`DaemonError::Ipc`] when the bind or permission hardening
/// fails.
#[cfg(unix)]
fn bind_socket(path: &Path) -> Result<EndpointListener, DaemonError> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(DaemonError::Io(error)),
    }
    UnixListenerAdapter::bind(path).map_err(DaemonError::Ipc)
}

/// Binds the named-pipe endpoint on Windows. There is no stale-path unlink:
/// the pipe namespace is kernel-managed, and the first-instance flag makes
/// a second daemon on the same pipe name fail closed at bind instead of
/// silently attaching to the live one.
///
/// # Errors
///
/// Returns [`DaemonError::Ipc`] when the pipe instance cannot be created.
#[cfg(windows)]
fn bind_socket(path: &Path) -> Result<EndpointListener, DaemonError> {
    NamedPipeListenerAdapter::bind(path, DAEMON_PIPE_INSTANCES, TransportConfig::default())
        .map_err(DaemonError::Ipc)
}

/// Bootstraps (or idempotently replays) the principal for one Ed25519 seed
/// file. Every request field is derived from the public key, so restarting
/// with the same key replays the same principal instead of minting a new
/// one; a different key under this daemon's derivation simply bootstraps
/// its own principal.
///
/// # Errors
///
/// Returns [`DaemonError::KeyFile`] for a malformed seed file and
/// [`DaemonError::Identity`] when the authority rejects the bootstrap.
fn bootstrap_principal(
    identity: &IdentityAuthority,
    key_file: &Path,
) -> Result<nlos_types::PrincipalId, DaemonError> {
    let seed = read_key_seed(key_file)?;
    let public_key = ed25519_dalek::SigningKey::from_bytes(&seed)
        .verifying_key()
        .to_bytes();
    let decision = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: digest32(
                b"llmos/system-control-daemon/principal-profile/v1",
                public_key,
            ),
            control_domain_policy_digest: digest32(
                b"llmos/system-control-daemon/control-domain-policy/v1",
                public_key,
            ),
            public_key,
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 0,
            key_valid_until_ms: BOOTSTRAP_KEY_VALID_UNTIL_MS,
            idempotency_key: IdempotencyKey::from_bytes(digest16(
                b"llmos/system-control-daemon/bootstrap-idempotency/v1",
                public_key,
            )),
            created_at_ms: 0,
        })
        .map_err(DaemonError::Identity)?;
    match decision {
        BootstrapDecision::Created(binding) | BootstrapDecision::Replayed(binding) => {
            Ok(binding.principal_id)
        }
    }
}

/// Reads the desktop-client key-file format: one file whose trimmed content
/// is exactly 64 hex characters (an Ed25519 seed).
///
/// # Errors
///
/// Returns [`DaemonError::KeyFile`] when the file cannot be read or is not
/// a 64-hex seed.
fn read_key_seed(key_file: &Path) -> Result<[u8; 32], DaemonError> {
    let invalid = || DaemonError::KeyFile("expected 64 hex characters of Ed25519 seed");
    let content =
        fs::read_to_string(key_file).map_err(|_| DaemonError::KeyFile("key file unreadable"))?;
    let raw = content.trim().as_bytes();
    if raw.len() != 64 || !raw.iter().all(u8::is_ascii_hexdigit) {
        return Err(invalid());
    }
    let mut seed = [0u8; 32];
    for (index, byte) in seed.iter_mut().enumerate() {
        let hi = (raw[2 * index] as char).to_digit(16).ok_or_else(invalid)?;
        let lo = (raw[2 * index + 1] as char)
            .to_digit(16)
            .ok_or_else(invalid)?;
        *byte = u8::try_from(hi * 16 + lo).map_err(|_| invalid())?;
    }
    Ok(seed)
}

fn digest32(domain: &[u8], public_key: [u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(public_key);
    hasher.finalize().into()
}

fn digest16(domain: &[u8], public_key: [u8; 32]) -> [u8; 16] {
    let digest = digest32(domain, public_key);
    let mut key = [0u8; 16];
    key.copy_from_slice(&digest[..16]);
    key
}

/// Handshake nonce random source: splitmix64 seeded from OS entropy at
/// assembly (fail-closed where a device read can fail), mirroring the
/// dev-fixture posture. Production wiring keeps everything inside the
/// process boundary; the seed never leaves it.
struct RandomSource {
    state: Arc<AtomicU64>,
}

impl RandomSource {
    /// # Errors
    ///
    /// Returns [`DaemonError::Io`] when the OS random source cannot be read;
    /// the daemon refuses to serve handshakes with a guessed seed.
    fn from_os() -> Result<Self, DaemonError> {
        Ok(Self {
            state: Arc::new(AtomicU64::new(os_entropy_seed()? | 1)),
        })
    }

    fn next_u64(&self) -> u64 {
        let mut value = self
            .state
            .fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
        value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }

    fn bytes32(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (index, chunk) in out.as_chunks_mut::<8>().0.iter_mut().enumerate() {
            let value = self.next_u64().wrapping_add((index as u64) << 32);
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        out
    }
}

/// Unix entropy seed: eight bytes read from `/dev/urandom` (fail-closed).
#[cfg(unix)]
fn os_entropy_seed() -> Result<u64, DaemonError> {
    use std::io::Read as _;

    let mut buffer = [0u8; 8];
    let mut source = fs::File::open("/dev/urandom").map_err(DaemonError::Io)?;
    source.read_exact(&mut buffer).map_err(DaemonError::Io)?;
    Ok(u64::from_le_bytes(buffer))
}

/// Windows entropy seed: no `/dev/urandom` device exists, so the splitmix64
/// state is seeded by mixing the std hasher's process-random OS seed, the
/// wall clock, and the pid through the same splitmix64 finalizer. Same
/// dev-fixture posture as the Unix half: the seed stays inside the process
/// and only has to be unpredictable per daemon start. The wall-clock read
/// below is **uniqueness/entropy mixing only** (F10/W61-A classification:
/// pure operational use) — it is never used as a timestamp value; every
/// authoritative time this daemon stamps comes from the `AuthorityClock`.
///
/// The `Result` signature is kept uniform with the Unix half (whose device
/// read fails closed) so [`RandomSource::from_os`] stays one un-gated call.
#[cfg(windows)]
#[allow(clippy::unnecessary_wraps)] // no Windows entropy source can fail
fn os_entropy_seed() -> Result<u64, DaemonError> {
    use std::collections::hash_map::RandomState;
    use std::hash::BuildHasher as _;
    use std::hash::Hasher as _;

    let mut seed = RandomState::new().build_hasher().finish();
    seed ^= u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default(),
    )
    .unwrap_or(u64::MAX);
    seed ^= u64::from(std::process::id());
    seed = (seed ^ (seed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    seed = (seed ^ (seed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    Ok(seed ^ (seed >> 31))
}

/// Operational monotonic-ns reading for the request-deadline freshness gate
/// (`deadline_monotonic_ns <= now` → `DeadlineExpired` in
/// `validate_sabi_request_context`). This is **not** an authoritative
/// timing source (F10/W61-A classification: pure operational measurement):
/// it never enters a durable record, receipt, or decision trail — the
/// `AuthorityClock`'s wall domain (via [`crate::auth::serve_validated`])
/// owns every authoritative timestamp both endpoints stamp. The clock
/// authority exposes no monotonic-ns domain (its tick domain is logical `+1`, its
/// wall domain is milliseconds), so the bare system clock remains the
/// deliberate source here.
fn monotonic_now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}
