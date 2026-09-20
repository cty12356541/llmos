//! 开发夹具(feature `dev-fixture`):供 `dev_server` 示例与集成测试共享。
//!
//! 在临时目录中装配与 `crates/nlos-system-control/tests/control_ipc_auth.rs`
//! 和 `control_command_cli.rs` 相同形态的真实权威——`IdentityAuthority`(真
//! Ed25519 验签)、`AuthorityClock`(真实系统墙钟 + SQLite 持久化)、
//! `SqliteTaskAuthority`(含一条 escalated 恢复计划)——并同时开放:
//!
//! - **认证入口**:ADR-0011 challenge-response(`authenticated_serve_one_control`),
//!   GUI 的唯一接线方式;
//! - **plain 入口**:`serve_one` + `handle_for_ipc`,供真实
//!   `system-control-cli` 二进制做一致性比对。
//!
//! 两个入口共享同一类 `RecoverySystemControl` handler(B-TASK-006L 三入口
//! 字节一致契约的服务侧前提)。夹具密钥为进程内随机生成(/dev/urandom 种子),
//! 只写入 `0600` 临时文件,绝不入仓库。本模块不承载任何生产语义。

use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nlos_clock::AuthorityClock;
use nlos_commit_coordinator::{
    RecoveryFailureAuthority, RecoveryWorkerFailure, RecoveryWorkerHealth, RecoveryWorkerState,
};
use nlos_identity::{BootstrapDecision, BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_ipc::handshake::transport::ServerHandshakeContext;
use nlos_ipc::unix::UnixListenerAdapter;
use nlos_ipc::{OutboundResponse, PeerAuthorizer, PeerIdentity, TransportConfig, serve_one};
use nlos_schema::sabi::v1::{ControlCommand as SabiWireCommand, ExchangeResponse};
use nlos_system_control::auth::authenticated_serve_one_control;
use nlos_system_control::control::{CONTROL_CAPABILITY_GENERATION, CONTROL_CAPABILITY_SLOT};
use nlos_system_control::{RecoveryHealthSource, RecoverySystemControl, SystemControlAuthorizer};
use nlos_task::{
    ArtifactCommitPlanId, ArtifactPublicationExpectation, ArtifactRecoveryFailureRequest,
    ArtifactRecoveryFailureSource, AttemptSpec, PermitDecision, PermitRequest,
    PlanArtifactCommitRequest, SnapshotBundle, SqliteTaskAuthority, artifact_publication_plan_root,
    empty_effect_history_root,
};
use nlos_types::{
    ArtifactId, CancellationScopeId, Generation, IdempotencyKey, TaskAttemptId, TaskId,
    TaskSnapshotId,
};
use tokio::task::JoinHandle;

/// 夹具装配失败(类型化;零 panic)。
#[derive(Debug)]
pub enum FixtureError {
    Io(std::io::Error),
    Identity(nlos_identity::IdentityAuthorityError),
    Clock(nlos_clock::AuthorityClockError),
    Task(String),
    Bind(nlos_ipc::IpcError),
    HandshakeContext(nlos_ipc::handshake::HandshakeError),
}

impl std::fmt::Display for FixtureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "fixture io: {error}"),
            Self::Identity(error) => write!(formatter, "fixture identity: {error}"),
            Self::Clock(error) => write!(formatter, "fixture clock: {error}"),
            Self::Task(reason) => write!(formatter, "fixture task authority: {reason}"),
            Self::Bind(error) => write!(formatter, "fixture bind: {error}"),
            Self::HandshakeContext(error) => {
                write!(formatter, "fixture handshake context: {error}")
            }
        }
    }
}

impl std::error::Error for FixtureError {}

/// 夹具内随机源:splitmix64,进程启动时以 /dev/urandom 播种。仅用于开发
/// 密钥与一次性握手 nonce 的生成;生产接线必须注入 OS 级 RNG。
#[derive(Clone)]
struct RandomSource {
    state: Arc<AtomicU64>,
}

impl RandomSource {
    fn from_os() -> Result<Self, FixtureError> {
        let mut buffer = [0u8; 8];
        read_os_random(&mut buffer)?;
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

fn read_os_random(buffer: &mut [u8]) -> Result<(), FixtureError> {
    use std::io::Read;

    let mut source = fs::File::open("/dev/urandom").map_err(FixtureError::Io)?;
    source.read_exact(buffer).map_err(FixtureError::Io)
}

fn wall_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

fn monotonic_now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
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

/// 临时根目录(夹具 Drop 时尽力清理)。
struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        Self(std::env::temp_dir().join(format!(
            "llmosdt-root-{label}-{}-{}",
            std::process::id(),
            NEXT_PATH.fetch_add(1, Ordering::Relaxed),
        )))
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

/// 短 socket 路径(macOS `SUN_LEN` 上限 104 字节)。
struct SocketPath(PathBuf);

impl SocketPath {
    fn new(label: &str) -> Self {
        Self(std::env::temp_dir().join(format!(
            "llmosdt-{label}-{}-{}.sock",
            std::process::id(),
            NEXT_PATH.fetch_add(1, Ordering::Relaxed),
        )))
    }
}

impl Deref for SocketPath {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<Path> for SocketPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// 与 control_ipc_auth.rs 相同形态的健康桩(不含任何敏感诊断)。
#[derive(Clone)]
pub struct StubHealth(RecoveryWorkerHealth);

impl RecoveryHealthSource for StubHealth {
    fn recovery_health(&self) -> RecoveryWorkerHealth {
        self.0.clone()
    }
}

/// 与测试 harness 相同的 capability 策略:只认控制面前缀的固定 handle 槽位。
struct CapabilityPolicy;

impl SystemControlAuthorizer for CapabilityPolicy {
    fn authorize_get(
        &self,
        context: &nlos_schema::sabi::v1::SabiRequestContext,
        _: &nlos_schema::sabi::v1::GetSystemControlRequest,
    ) -> Result<(), &'static str> {
        authorize_capability(context)
    }

    fn authorize_submit(
        &self,
        context: &nlos_schema::sabi::v1::SabiRequestContext,
        _: &SabiWireCommand,
    ) -> Result<(), &'static str> {
        authorize_capability(context)
    }
}

fn authorize_capability(
    context: &nlos_schema::sabi::v1::SabiRequestContext,
) -> Result<(), &'static str> {
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

struct AllowPeer;

impl PeerAuthorizer for AllowPeer {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

/// 装配一条 escalated artifact 恢复计划(与 control_ipc_auth.rs 的
/// `create_escalated_plan` 相同的权威调用序列)。
#[allow(deprecated)] // 与 control_ipc_auth.rs 同口径:夹具构造走 legacy ladder 构造器。
fn create_escalated_plan(
    authority: &SqliteTaskAuthority,
) -> Result<ArtifactCommitPlanId, FixtureError> {
    let fail = |reason: &str| FixtureError::Task(reason.to_owned());
    let task_id = TaskId::from_bytes([0x11; 16]);
    authority
        .register_task(nlos_task::TaskSpec {
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .map_err(|_| fail("register_task"))?;
    let attempt = AttemptSpec {
        task_id,
        attempt_id: TaskAttemptId::from_bytes([0x12; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([0x13; 16]),
            snapshot_digest: [0x14; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0x15; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0x16; 16]),
        registered_at_ms: 2_000,
    };
    authority
        .register_attempt(attempt)
        .map_err(|_| fail("register_attempt"))?;
    let expectation = ArtifactPublicationExpectation {
        staging_id: [0x21; 16],
        artifact_id: ArtifactId::from_bytes([0x22; 16]),
        target_revision: 1,
        digest: [0x23; 32],
        size_bytes: 10,
    };
    let PermitDecision::Issued(permit) = authority
        .request_commit_permit(PermitRequest {
            task_id,
            attempt_id: attempt.attempt_id,
            attempt_generation: attempt.attempt_generation,
            write_set_root: artifact_publication_plan_root(std::slice::from_ref(&expectation))
                .map_err(|_| fail("plan_root"))?,
            planned_effects: Vec::new(),
            idempotency_key: IdempotencyKey::from_bytes([0x17; 16]),
            valid_until_ms: 20_000,
            requested_at_ms: 3_000,
        })
        .map_err(|_| fail("commit_permit"))?
    else {
        return Err(fail("expected permit"));
    };
    let plan = authority
        .plan_artifact_commit(PlanArtifactCommitRequest {
            task_id,
            attempt_id: attempt.attempt_id,
            attempt_generation: attempt.attempt_generation,
            permit_id: permit.permit_id,
            idempotency_key: IdempotencyKey::from_bytes([0x18; 16]),
            expectations: vec![expectation],
            planned_at_ms: 4_000,
        })
        .map_err(|_| fail("plan_artifact_commit"))?
        .record()
        .clone();
    authority
        .record_artifact_recovery_failure(ArtifactRecoveryFailureRequest {
            plan_id: plan.plan_id,
            expected_total_failures: 0,
            source: ArtifactRecoveryFailureSource::ArtifactAuthority,
            observed_at_ms: 5_000,
            base_delay_ms: 100,
            max_delay_ms: 1_000,
            escalation_threshold: 1,
        })
        .map_err(|_| fail("record_recovery_failure"))?;
    Ok(plan.plan_id)
}

/// 一次性开发夹具:双入口(认证/plain)服务 + 客户端接线参数。
pub struct DevFixture {
    /// keepalive:TempRoot 只为借用其 Drop(夹具释放时清理临时目录)。
    #[allow(dead_code)]
    root: TempRoot,
    socket_authenticated: SocketPath,
    socket_plain: SocketPath,
    tasks: Arc<SqliteTaskAuthority>,
    identity: Arc<IdentityAuthority>,
    clock: Arc<AuthorityClock>,
    handshake: Arc<ServerHandshakeContext>,
    health: StubHealth,
    listener_authenticated: Option<UnixListenerAdapter>,
    listener_plain: Option<UnixListenerAdapter>,
    random: RandomSource,
    principal_hex: String,
    key_seed_hex: String,
    plan_id_hex: String,
}

impl DevFixture {
    /// 装配全部真实权威并绑定两个 Unix socket(label 仅用于临时路径命名,≤3 字符)。
    pub fn spawn(label: &str) -> Result<Self, FixtureError> {
        let random = RandomSource::from_os()?;
        let key_seed = random.bytes32();
        let root = TempRoot::new(label);
        let identity =
            IdentityAuthority::open(root.0.join("identity")).map_err(FixtureError::Identity)?;
        let public_key = ed25519_dalek::SigningKey::from_bytes(&key_seed)
            .verifying_key()
            .to_bytes();
        let BootstrapDecision::Created(binding) = identity
            .bootstrap_principal(BootstrapPrincipalRequest {
                principal_profile_digest: [0xA1; 32],
                control_domain_policy_digest: [0xA2; 32],
                public_key,
                key_purpose: KeyPurpose::SemanticSigning,
                key_valid_from_ms: 0,
                // 开发演示窗口:到 2100-01-01(非机密,仅有效期上限)。
                key_valid_until_ms: 4_102_444_800_000,
                idempotency_key: IdempotencyKey::from_bytes([0xA3; 16]),
                created_at_ms: 0,
            })
            .map_err(FixtureError::Identity)?
        else {
            return Err(FixtureError::Task("fresh authority bootstraps".to_owned()));
        };

        let clock = AuthorityClock::open(root.0.join("clock")).map_err(FixtureError::Clock)?;
        let tasks = Arc::new(
            SqliteTaskAuthority::open(root.0.join("tasks.sqlite3"))
                .map_err(|_| FixtureError::Task("open task authority".to_owned()))?,
        );
        let plan_id = create_escalated_plan(tasks.as_ref())?;
        let health = StubHealth(RecoveryWorkerHealth {
            state: RecoveryWorkerState::BackingOff,
            completed_cycles: 4,
            total_inspected: 3,
            total_finalized: 2,
            consecutive_failed_cycles: 0,
            retry_delay: Some(Duration::from_millis(250)),
            last_failures: vec![RecoveryWorkerFailure {
                plan_id: Some(plan_id),
                authority: RecoveryFailureAuthority::Artifact,
                message: "dev fixture: artifact commit recovery failed".to_owned(),
            }],
            durable_retrying: 0,
            durable_escalated: 1,
            durable_unacknowledged_escalated: 1,
            durable_resolved: 0,
            semantic_durable_retrying: 0,
            semantic_durable_escalated: 0,
            semantic_durable_unacknowledged_escalated: 0,
            semantic_durable_resolved: 0,
            semantic_consecutive_failed_cycles: 0,
            semantic_total_inspected: 0,
            semantic_total_finalized: 0,
            semantic_domain_faulted: false,
            artifact_domain_faulted: false,
        });

        let socket_authenticated = SocketPath::new(&format!("{label}a"));
        let socket_plain = SocketPath::new(&format!("{label}p"));
        let handshake = Arc::new(
            ServerHandshakeContext::new(&socket_authenticated, 64)
                .map_err(FixtureError::HandshakeContext)?,
        );
        let listener_authenticated =
            Some(UnixListenerAdapter::bind(&socket_authenticated).map_err(FixtureError::Bind)?);
        let listener_plain =
            Some(UnixListenerAdapter::bind(&socket_plain).map_err(FixtureError::Bind)?);

        Ok(Self {
            root,
            socket_authenticated,
            socket_plain,
            tasks,
            identity: Arc::new(identity),
            clock: Arc::new(clock),
            handshake,
            health,
            listener_authenticated,
            listener_plain,
            random,
            principal_hex: hex(binding.principal_id.as_bytes()),
            key_seed_hex: hex(&key_seed),
            plan_id_hex: hex(plan_id.as_bytes()),
        })
    }

    #[must_use]
    pub fn socket_authenticated(&self) -> &Path {
        &self.socket_authenticated
    }

    #[must_use]
    pub fn socket_plain(&self) -> &Path {
        &self.socket_plain
    }

    #[must_use]
    pub fn principal_hex(&self) -> &str {
        &self.principal_hex
    }

    #[must_use]
    pub fn key_seed_hex(&self) -> &str {
        &self.key_seed_hex
    }

    #[must_use]
    pub fn plan_id_hex(&self) -> &str {
        &self.plan_id_hex
    }

    /// 把 Ed25519 种子写入 `0600` 密钥文件(客户端派发时读取)。
    pub fn write_key_file(&self, path: &Path) -> Result<(), FixtureError> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(FixtureError::Io)?;
        file.write_all(self.key_seed_hex.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(FixtureError::Io)?;
        Ok(())
    }

    /// 启动两个服务循环(认证入口 + plain 入口),返回各自的 JoinHandle。
    pub fn serve_forever(&mut self) -> (JoinHandle<()>, JoinHandle<()>) {
        let authenticated = self.spawn_authenticated_loop();
        let plain = self.spawn_plain_loop();
        (authenticated, plain)
    }

    fn spawn_authenticated_loop(&mut self) -> JoinHandle<()> {
        let listener = self.listener_authenticated.take();
        let tasks = Arc::clone(&self.tasks);
        let identity = Arc::clone(&self.identity);
        let clock = Arc::clone(&self.clock);
        let handshake = Arc::clone(&self.handshake);
        let health = self.health.clone();
        let random = self.random.clone();
        tokio::spawn(async move {
            let Some(listener) = listener else {
                return;
            };
            loop {
                let control =
                    RecoverySystemControl::new(tasks.as_ref(), &health, &CapabilityPolicy);
                let outcome = authenticated_serve_one_control(
                    &listener,
                    TransportConfig::default(),
                    &control,
                    identity.as_ref(),
                    clock.as_ref(),
                    handshake.as_ref(),
                    &AllowPeer,
                    monotonic_now_ns(),
                    || random.bytes32(),
                )
                .await;
                if let Err(error) = outcome {
                    // 空闲 accept 超时与对端握手失败都是正常轮次;其余错误
                    // 退避后继续,避免热旋(夹具服务直到进程退出)。
                    eprintln!("dev_server: authenticated accept/handshake: {error}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        })
    }

    fn spawn_plain_loop(&mut self) -> JoinHandle<()> {
        let listener = self.listener_plain.take();
        let tasks = Arc::clone(&self.tasks);
        let health = self.health.clone();
        tokio::spawn(async move {
            let Some(listener) = listener else {
                return;
            };
            loop {
                let (stream, peer) = match listener.accept(TransportConfig::default()).await {
                    Ok(connection) => connection,
                    Err(nlos_ipc::IpcError::Timeout(nlos_ipc::IoOperation::Accept)) => continue,
                    Err(error) => {
                        eprintln!("dev_server: plain accept: {error}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let wall_ms = i64::try_from(wall_now_ms()).unwrap_or(i64::MAX);
                // 一个坏交换不拖垮端点:handler 失败只影响该连接。
                let _ = serve_one(stream, TransportConfig::default(), peer, &AllowPeer, {
                    let tasks = Arc::clone(&tasks);
                    let health = health.clone();
                    move |validated| {
                        let control =
                            RecoverySystemControl::new(tasks.as_ref(), &health, &CapabilityPolicy);
                        let response = control.handle_for_ipc(
                            validated.envelope(),
                            monotonic_now_ns(),
                            wall_ms,
                        );
                        async {
                            Ok(OutboundResponse::Typed(ExchangeResponse {
                                envelope: Some(response),
                            }))
                        }
                    }
                })
                .await;
            }
        })
    }
}
