//! Resident `system_control` daemon (`daemon` feature, Unix only).
//!
//! Decision D5: this module closes the assembly gap between the desktop GUI
//! authenticated entry and the CLI plain entry when no desktop service
//! process is running. One daemon owns a state root, opens every real
//! authority under it (identity, clock, task, artifact, process, resource,
//! application, plan, channel+topic, operation store, and a tokio runtime
//! adapter), starts the [`TaskAuthorityCommitRecoveryWorker`], and serves
//! two Unix socket endpoints side by side:
//!
//! - **authenticated entry** — [`authenticated_serve_one_control`] (the GUI's
//!   only wiring shape; every connection answers the ADR-0011
//!   challenge-response handshake);
//! - **plain entry** — `serve_one` + [`RecoverySystemControl::handle_for_ipc`]
//!   for the `system-control-cli` binary.
//!
//! Both entries share one handler construction path with every
//! `with_*_source` inspector seam wired; the executor arms stay
//! [`crate::UnwiredOperationCommandExecutor`]-shaped (no executor is
//! injected), so pause/resume/cancel/kill/throttle/reclaim and the
//! application lifecycle arms refuse fail-closed by construction. The
//! daemon is a read-only inspection and recovery-command surface.
//!
//! The `Process`/`Resource`/`Application` authorities are opened because the
//! daemon is the resident owner of the state root (and the worker drives the
//! resource half through its own `ResourceAuthority` handle); their
//! inspector adapters are composed **client-side** per the
//! `ControlReceipt::compose` contract, exactly like the CLI `--root` path.
//!
//! Process/resource/application inspection therefore never crosses these
//! sockets as server-side state; the GET envelope still crosses for
//! authorization parity.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io::Read;
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
use nlos_ipc::unix::UnixListenerAdapter;
use nlos_ipc::{
    OutboundResponse, PeerAuthorizer, PeerIdentity, TransportConfig, handshake::HandshakeError,
    serve_one,
};
use nlos_plan::{PlanStoreError, SqlitePlanAuthority};
use nlos_process::{ProcessAuthority, ProcessAuthorityError};
use nlos_resource::{ResourceAuthority, ResourceAuthorityError};
use nlos_runtime::RuntimeError;
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig};
use nlos_schema::sabi::v1::{
    ControlCommand as SabiWireCommand, ExchangeResponse, GetSystemControlRequest,
    SabiRequestContext,
};
use nlos_store::{SqliteOperationStore, StoreError};
use nlos_task::{SqliteTaskAuthority, TaskStoreError};
use nlos_topic::{TopicAuthority, TopicAuthorityError};
use nlos_types::IdempotencyKey;
use sha2::{Digest, Sha256};

use crate::auth::authenticated_serve_one_control;
use crate::control::{
    CONTROL_CAPABILITY_GENERATION, CONTROL_CAPABILITY_SLOT, ControlCommand, ControlError,
    ControlReceipt,
};
use crate::fiber_inspector::TokioExecutionFiberSource;
use crate::operation_inspector::OperationStoreSource;
use crate::plan_inspector::PlanAuthorityTaskNodeSource;
use crate::topic_inspector::TopicAuthoritySource;
use crate::{RecoveryHealthSource, RecoverySystemControl, SystemControlAuthorizer};

/// Key validity ceiling for daemon-bootstrapped principals: 2100-01-01. Not
/// a secret; only an upper bound, mirroring the dev-fixture window.
const BOOTSTRAP_KEY_VALID_UNTIL_MS: u64 = 4_102_444_800_000;
/// Handshake nonce registry capacity for the authenticated endpoint.
const HANDSHAKE_NONCE_CAPACITY: usize = 64;

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
            Self::KeyFile(_) => None,
        }
    }
}

/// Daemon startup options.
pub struct DaemonOptions {
    /// State root every authority is opened under.
    pub root: PathBuf,
    /// Authenticated endpoint path; defaults to
    /// `<root>/system-control-auth.sock`.
    pub auth_socket: Option<PathBuf>,
    /// Plain endpoint path; defaults to `<root>/system-control-plain.sock`.
    pub plain_socket: Option<PathBuf>,
    /// Optional 64-hex Ed25519 seed file (the desktop client's 0600 key-file
    /// format). When present the daemon bootstraps (or idempotently replays)
    /// the matching principal in the identity authority so a client holding
    /// the same seed can authenticate.
    pub identity_key_file: Option<PathBuf>,
    /// Recovery worker tuning; the first scan runs immediately on start.
    pub worker_config: RecoveryWorkerConfig,
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
        }
    }

    /// Overrides the authenticated endpoint path.
    #[must_use]
    pub fn with_auth_socket(mut self, path: impl Into<PathBuf>) -> Self {
        self.auth_socket = Some(path.into());
        self
    }

    /// Overrides the plain endpoint path.
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

    fn auth_socket_path(&self) -> PathBuf {
        self.auth_socket
            .clone()
            .unwrap_or_else(|| self.root.join("system-control-auth.sock"))
    }

    fn plain_socket_path(&self) -> PathBuf {
        self.plain_socket
            .clone()
            .unwrap_or_else(|| self.root.join("system-control-plain.sock"))
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
/// endpoint stays inside the local trust domain (filesystem-permission
/// 0600 sockets), so the in-transport peer gate admits and the capability
/// policy remains the boundary.
struct AllowPeer;

impl PeerAuthorizer for AllowPeer {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
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
    /// Process authority (resident owner; client-side inspector surface).
    pub process: Arc<ProcessAuthority>,
    /// Resource authority (worker resource half + client-side inspector).
    pub resource: Arc<ResourceAuthority>,
    /// Application authority (resident owner; client-side inspector surface).
    pub application: Arc<ApplicationAuthority>,
    /// Plan authority behind the `TaskNode` inspect seam.
    pub plans: SqlitePlanAuthority,
    /// Channel authority the topic authority is bound to.
    pub channel: Arc<ChannelAuthority>,
    /// Topic authority behind the `Topic` inspect seam.
    pub topics: TopicAuthority,
    /// Operation store behind the `Operation` inspect seam.
    pub operations: SqliteOperationStore,
    /// Live runtime adapter behind the `ExecutionFiber` inspect seam.
    pub runtime: TokioRuntimeAdapter,
    /// Handshake nonce registry bound to the authenticated endpoint.
    pub handshake: Arc<ServerHandshakeContext>,
    /// One-time handshake nonce source, seeded from the OS.
    random: RandomSource,
    /// Running recovery worker (health face + idempotent stop).
    worker: Arc<SharedRecoveryWorker>,
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

    /// Wires the handler with every layer inspection source. The sources
    /// must outlive the returned handler; both service loops and
    /// [`Self::dispatch_command`] build them per exchange.
    fn layer_control<'a>(
        &'a self,
        plans: &'a PlanAuthorityTaskNodeSource<'a>,
        fibers: &'a TokioExecutionFiberSource<'a>,
        topics: &'a TopicAuthoritySource<'a>,
        operations: &'a OperationStoreSource<'a>,
    ) -> RecoverySystemControl<'a, SharedRecoveryWorker, ControlCapabilityPolicy> {
        RecoverySystemControl::new(
            self.tasks.as_ref(),
            self.worker.as_ref(),
            &ControlCapabilityPolicy,
        )
        .with_task_node_source(plans)
        .with_execution_fiber_source(fibers)
        .with_topic_source(topics)
        .with_operation_source(operations)
    }

    /// Dispatches one [`ControlCommand`] in-process through the daemon's own
    /// handler (service parity face for tests and health probes). The
    /// client-composed inspector slots are left unwired here: process,
    /// resource, and application inspection is composed by the caller.
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
        let control = self.layer_control(&plans, &fibers, &topics, &operations);
        crate::control::dispatch_in_process(
            &control,
            command,
            now_monotonic_ns,
            now_wall_ms,
            None,
            None,
            None,
        )
    }
}

/// Bound listeners for the two endpoints, handed to the service loops.
pub struct DaemonEndpoints {
    pub listener_authenticated: UnixListenerAdapter,
    pub listener_plain: UnixListenerAdapter,
}

/// Assembles the daemon: opens every authority under `options.root`, starts
/// the recovery worker (semantic half quiescent — no semantic authority is
/// opened; the artifact and resource halves run), optionally bootstraps the
/// `--identity-key-file` principal, and binds both endpoints.
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
    let tasks = Arc::new(
        SqliteTaskAuthority::open(options.root.join("tasks.sqlite3")).map_err(DaemonError::Task)?,
    );
    let artifacts = Arc::new(
        ArtifactStore::open(options.root.join("artifacts")).map_err(DaemonError::Artifact)?,
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
    let plans =
        SqlitePlanAuthority::open(options.root.join("plans.sqlite3")).map_err(DaemonError::Plan)?;
    let channel = Arc::new(
        ChannelAuthority::open(options.root.join("channel")).map_err(DaemonError::Channel)?,
    );
    let topics = TopicAuthority::open(options.root.join("topics"), Arc::clone(&channel))
        .map_err(DaemonError::Topic)?;
    let operations = SqliteOperationStore::open(options.root.join("operations.sqlite3"))
        .map_err(DaemonError::Store)?;
    let runtime = TokioRuntimeAdapter::new(runtime_handle, TokioRuntimeConfig::default())
        .map_err(DaemonError::Runtime)?;
    // The worker's semantic half stays quiescent: the daemon opens no
    // semantic authority, so only artifact and resource plans are scanned.
    let worker = TaskAuthorityCommitRecoveryWorker::start_with_semantic_and_resource_authorities(
        Arc::clone(&tasks),
        Arc::clone(&artifacts),
        None,
        Some(Arc::clone(&resource)),
        options.worker_config,
    )
    .map_err(DaemonError::Worker)?;
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
        clock: Arc::new(clock),
        tasks,
        artifacts,
        process,
        resource,
        application,
        plans,
        channel,
        topics,
        operations,
        runtime,
        handshake,
        random,
        worker: Arc::new(SharedRecoveryWorker(Mutex::new(worker))),
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
    listener: UnixListenerAdapter,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        let plans = PlanAuthorityTaskNodeSource::new(&daemon.plans);
        let fibers = TokioExecutionFiberSource::new(&daemon.runtime);
        let topics = TopicAuthoritySource::new(&daemon.topics);
        let operations = OperationStoreSource::new(&daemon.operations);
        let control = daemon.layer_control(&plans, &fibers, &topics, &operations);
        let outcome = authenticated_serve_one_control(
            &listener,
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
/// # Panics
///
/// Never panics by construction; every failure path is a logged round.
pub async fn serve_plain_endpoint(
    daemon: Arc<SystemControlDaemon>,
    listener: UnixListenerAdapter,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        let (stream, peer) = match listener.accept(TransportConfig::default()).await {
            Ok(connection) => connection,
            Err(nlos_ipc::IpcError::Timeout(nlos_ipc::IoOperation::Accept)) => continue,
            Err(error) => {
                eprintln!("system-control-daemon: plain accept: {error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let wall_ms = wall_now_ms();
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
                    let control = daemon.layer_control(&plans, &fibers, &topics, &operations);
                    let response =
                        control.handle_for_ipc(validated.envelope(), monotonic_now_ns(), wall_ms);
                    Ok(OutboundResponse::Typed(ExchangeResponse {
                        envelope: Some(response),
                    }))
                }
            },
        )
        .await;
    }
}

/// Removes a stale socket path (best effort — a live daemon keeps serving
/// its already-bound inode) and binds a fresh owner-only endpoint.
///
/// # Errors
///
/// Returns [`DaemonError::Ipc`] when the bind or permission hardening
/// fails.
fn bind_socket(path: &Path) -> Result<UnixListenerAdapter, DaemonError> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(DaemonError::Io(error)),
    }
    UnixListenerAdapter::bind(path).map_err(DaemonError::Ipc)
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

/// Handshake nonce random source: splitmix64 seeded from `/dev/urandom` at
/// assembly (fail-closed), mirroring the dev-fixture posture. Production
/// wiring keeps everything inside the process boundary; the seed never
/// leaves it.
struct RandomSource {
    state: Arc<AtomicU64>,
}

impl RandomSource {
    /// # Errors
    ///
    /// Returns [`DaemonError::Io`] when the OS random source cannot be read;
    /// the daemon refuses to serve handshakes with a guessed seed.
    fn from_os() -> Result<Self, DaemonError> {
        let mut buffer = [0u8; 8];
        let mut source = fs::File::open("/dev/urandom").map_err(DaemonError::Io)?;
        source.read_exact(&mut buffer).map_err(DaemonError::Io)?;
        Ok(Self {
            state: Arc::new(AtomicU64::new(u64::from_le_bytes(buffer) | 1)),
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
        for (index, chunk) in out.chunks_exact_mut(8).enumerate() {
            let value = self.next_u64().wrapping_add((index as u64) << 32);
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        out
    }
}

fn wall_now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default(),
    )
    .unwrap_or(i64::MAX)
}

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
