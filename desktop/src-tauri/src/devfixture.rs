//! 开发夹具(feature `dev-fixture`):供 `dev_server` 示例与集成测试共享。
//!
//! 在临时目录中装配与 `crates/nlos-system-control/tests/control_ipc_auth.rs`
//! 和 `control_command_cli.rs` 相同形态的真实权威——`IdentityAuthority`(真
//! Ed25519 验签)、`AuthorityClock`(真实系统墙钟 + SQLite 持久化)、
//! `SqliteTaskAuthority`(含一条 escalated 恢复计划)——并同时开放:
//!
//! - **认证入口**:ADR-0011 challenge-response(GUI 的唯一接线方式);
//! - **plain 入口**:`serve_one` + `handle_for_ipc`,供一致性比对
//!   (Unix:真实 `system-control-cli` 二进制;Windows:`dispatch_plain`
//!   测试助手——CLI 二进制本身仍是 Unix-only)。
//!
//! 传输面按平台分派(F3-c2):Unix 走 `nlos-ipc` 的 Unix domain socket
//! 适配器与上游 `authenticated_serve_one_control`(W32 起路径零改动);
//! Windows 走 `nlos-ipc` 的 named pipe 适配器 + 平台中立握手原语在
//! devfixture 侧装配的等价服务回路(上游 auth 模块整体 cfg(unix))。
//!
//! OS 凭据面(F3-c3):Windows 侧 named pipe 对端 pid 由 `nlos-ipc` 解析
//! (`GetNamedPipeClientProcessId`),服务端以 `SameUserAuthorizer` 做
//! 同用户令牌门(0600 Unix socket 权限位的 Windows 等价物:仅创建用户
//! 的进程可入);Unix 夹具维持 `AllowPeer` 现状(生产凭据门在上游
//! daemon,夹具语义两平台对齐:认证入口另有 ADR-0011 principal 级握手)。
//!
//! 两个入口共享同一类 `RecoverySystemControl` handler(B-TASK-006L 三入口
//! 字节一致契约的服务侧前提)。夹具密钥为进程内随机生成(OS 随机源种子),
//! 只写入权限收窄的临时文件,绝不入仓库。本模块不承载任何生产语义。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nlos_clock::AuthorityClock;
use nlos_commit_coordinator::{
    RecoveryFailureAuthority, RecoveryWorkerFailure, RecoveryWorkerHealth, RecoveryWorkerState,
};
use nlos_identity::{BootstrapDecision, BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
#[cfg(unix)]
use nlos_ipc::handshake::transport::ServerHandshakeContext;
#[cfg(unix)]
use nlos_ipc::unix::UnixListenerAdapter;
#[cfg(windows)]
use nlos_ipc::windows::NamedPipeListenerAdapter;
use nlos_ipc::{OutboundResponse, PeerAuthorizer, PeerIdentity, TransportConfig, serve_one};
use nlos_resource::{
    ActivateReservationRequest, ActivationDecision, ConsumeReservationRequest,
    CreateAccountRequest, CreateQuoteRequest, FinalizeDecision, FinalizeReservationRequest,
    RegisterDriverRequest, ReserveRequest, ResourceAuthority, ResourceDemand,
};
use nlos_schema::sabi::v1::{ControlCommand as SabiWireCommand, ExchangeResponse};
#[cfg(unix)]
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
    ArtifactId, CallId, CancellationScopeId, Generation, IdempotencyKey, OperationId,
    TaskAttemptId, TaskId, TaskSnapshotId,
};
use tokio::task::JoinHandle;

/// 夹具监听器(cfg 分派:Unix domain socket / Windows named pipe)。
#[cfg(unix)]
type ListenerAdapter = UnixListenerAdapter;
#[cfg(windows)]
type ListenerAdapter = NamedPipeListenerAdapter;

/// 服务端握手上下文(Unix:上游 `ServerHandshakeContext`;
/// Windows:devfixture 侧等价物,见 [`WindowsHandshakeContext`])。
#[cfg(unix)]
type HandshakeServerContext = ServerHandshakeContext;
#[cfg(windows)]
type HandshakeServerContext = WindowsHandshakeContext;

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
        for index in 0..4 {
            let value = self.next_u64().wrapping_add((index as u64) << 32);
            out[index * 8..index * 8 + 8].copy_from_slice(&value.to_le_bytes());
        }
        out
    }
}

fn read_os_random(buffer: &mut [u8]) -> Result<(), FixtureError> {
    #[cfg(unix)]
    {
        use std::io::Read;

        let mut source = fs::File::open("/dev/urandom").map_err(FixtureError::Io)?;
        source.read_exact(buffer).map_err(FixtureError::Io)
    }
    #[cfg(windows)]
    {
        getrandom::fill(buffer).map_err(|error| FixtureError::Io(std::io::Error::other(error)))
    }
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

/// 夹具端点地址。Unix:短 socket 路径(macOS `SUN_LEN` 上限 104 字节,
/// Drop 清理 socket 文件);Windows:`\\.\pipe\` 本地管道名(全句柄关闭后
/// 命名空间自动回收,无需清理)。
struct FixtureEndpoint {
    #[cfg(unix)]
    path: PathBuf,
    #[cfg(windows)]
    name: String,
}

impl FixtureEndpoint {
    fn new(label: &str) -> Self {
        #[cfg(unix)]
        {
            Self {
                path: std::env::temp_dir().join(format!(
                    "llmosdt-{label}-{}-{}.sock",
                    std::process::id(),
                    NEXT_PATH.fetch_add(1, Ordering::Relaxed),
                )),
            }
        }
        #[cfg(windows)]
        {
            Self {
                name: format!(
                    r"\\.\pipe\llmosdt-{label}-{}-{}",
                    std::process::id(),
                    NEXT_PATH.fetch_add(1, Ordering::Relaxed),
                ),
            }
        }
    }

    /// 端点地址字符串(GUI 会话配置 / dispatch 入参形态)。
    fn to_endpoint_string(&self) -> String {
        #[cfg(unix)]
        {
            self.path.display().to_string()
        }
        #[cfg(windows)]
        {
            self.name.clone()
        }
    }
}

#[cfg(unix)]
impl AsRef<Path> for FixtureEndpoint {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
impl Drop for FixtureEndpoint {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// 绑定一个夹具端点(Unix:0600 权限位收窄;Windows:named pipe,
/// `reject_remote_clients`,顺序服务 4 实例足够)。
fn bind_endpoint(endpoint: &FixtureEndpoint) -> Result<ListenerAdapter, FixtureError> {
    #[cfg(unix)]
    {
        UnixListenerAdapter::bind(endpoint).map_err(FixtureError::Bind)
    }
    #[cfg(windows)]
    {
        NamedPipeListenerAdapter::bind(&endpoint.name, 4, TransportConfig::default())
            .map_err(FixtureError::Bind)
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

/// Unix 夹具 peer 门(F3-c3 起 Windows 装配 `SameUserAuthorizer`,此桩
/// 仅 Unix 保留——与上游夹具现状一致,生产凭据门在 daemon)。
#[cfg(unix)]
struct AllowPeer;

#[cfg(unix)]
impl PeerAuthorizer for AllowPeer {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

/// 密钥文件创建:两端平台都在落盘时显式收窄访问面——Unix 以 0600 权限位;
/// Windows 以 owner-only DACL(`crate::winsec`:唯一 allow-ACE = 当前用户
/// `FILE_ALL_ACCESS`,创建时经 `SECURITY_ATTRIBUTES` 一次到位,不存在默认
/// DACL 存续窗口;`key_file_acl_side.rs` 钉死其结构)。
fn open_key_file(path: &Path) -> Result<fs::File, FixtureError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(FixtureError::Io)
    }
    #[cfg(windows)]
    {
        crate::winsec::create_new_owner_only(path).map_err(FixtureError::Io)
    }
}

// ---------------------------------------------------------------------------
// Windows named pipe 服务侧(F3-c2)
// ---------------------------------------------------------------------------

/// Windows 服务端握手上下文:上游 `ServerHandshakeContext` 的 devfixture
/// 等价物(上游 `handshake::transport` 整体 cfg(unix))。绑定派生与客户端
/// 共用 [`crate::transport::endpoint_channel_binding`]。
#[cfg(windows)]
struct WindowsHandshakeContext {
    binding: [u8; 32],
    nonces: nlos_ipc::handshake::HandshakeNonceRegistry,
}

#[cfg(windows)]
impl WindowsHandshakeContext {
    fn new(
        endpoint: &str,
        nonce_capacity: usize,
    ) -> Result<Self, nlos_ipc::handshake::HandshakeError> {
        let normalized = crate::transport::normalize_endpoint(endpoint).map_err(|_| {
            nlos_ipc::handshake::HandshakeError::InvalidConfig(
                "named pipe endpoint must be \\\\.\\pipe\\<name>",
            )
        })?;
        Ok(Self {
            binding: crate::transport::endpoint_channel_binding(&normalized),
            nonces: nlos_ipc::handshake::HandshakeNonceRegistry::new(nonce_capacity)?,
        })
    }

    fn nonces(&self) -> &nlos_ipc::handshake::HandshakeNonceRegistry {
        &self.nonces
    }
}

/// named pipe accept:F3-c3 起 `nlos-ipc` 适配器的 accept 超时是
/// cancel-safe 的(超时实例连同挂起的 ConnectNamedPipe 一并归还 listener,
/// 下轮复用),夹具不再需要放宽超时预算规避"超时丢弃预创建实例且不重建"
/// 的旧行为——空闲轮按 `TransportConfig` 的 connect 预算轮转,与 Unix
/// 夹具的空闲轮语义一致。
#[cfg(windows)]
async fn accept_pipe(
    listener: &mut NamedPipeListenerAdapter,
    config: TransportConfig,
) -> Result<
    (
        tokio::net::windows::named_pipe::NamedPipeServer,
        PeerIdentity,
    ),
    nlos_ipc::IpcError,
> {
    listener.accept(config).await
}

/// Windows 版 `authenticated_serve_one_control` 的 devfixture 镜像:语义逐
/// 步对齐上游(`nlos-system-control` auth.rs,整体 cfg(unix))——
/// 时钟签发的握手验证时间 → accept → peer 门 → challenge → 验证 attestation
/// → `serve_one`(墙钟经 `AuthorityClock`,按 correlation id 幂等)。
/// 上游把 post-handshake serve 结果放在 `AuthenticatedServeOutcome.served`
/// (不算握手错误);夹具循环同样忽略 serve 结果,只对握手错误退避。
/// peer 门为泛型(F3-c3:Windows 装配 `SameUserAuthorizer`)。
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
async fn authenticated_serve_one_control_pipe<H, A, G, N>(
    listener: &mut NamedPipeListenerAdapter,
    config: TransportConfig,
    control: &RecoverySystemControl<'_, H, A>,
    identity: &IdentityAuthority,
    clock: &AuthorityClock,
    handshake: &WindowsHandshakeContext,
    peer_gate: &G,
    now_monotonic_ns: u64,
    next_nonce: N,
) -> Result<(), nlos_ipc::handshake::HandshakeError>
where
    H: RecoveryHealthSource,
    A: SystemControlAuthorizer,
    G: PeerAuthorizer,
    N: FnOnce() -> [u8; nlos_schema::HANDSHAKE_NONCE_BYTES],
{
    use nlos_clock::NowRequest;
    use nlos_identity::IdentityAuthorityError;
    use nlos_ipc::FramedIo;
    use nlos_ipc::handshake::{
        HandshakeError, decode_attestation_wire, encode_challenge_wire, issue_challenge,
        verify_attestation,
    };

    let nonce = next_nonce();
    let verified_at_ms = clock
        .wall_now(NowRequest {
            idempotency_key: handshake_wall_key(&nonce),
        })
        .map_err(|error| HandshakeError::Identity(IdentityAuthorityError::Clock(error)))?
        .reading()
        .as_u64();

    let (stream, peer) = accept_pipe(listener, config)
        .await
        .map_err(HandshakeError::Transport)?;
    peer_gate
        .authorize(&peer)
        .map_err(HandshakeError::PeerAuthorization)?;

    let mut framed = FramedIo::new(stream, config);
    let challenge = issue_challenge(handshake.nonces(), nonce)?;
    framed
        .send(&encode_challenge_wire(&challenge)?)
        .await
        .map_err(HandshakeError::Transport)?;
    let attestation_wire = framed.receive().await.map_err(HandshakeError::Transport)?;
    let attestation = decode_attestation_wire(&attestation_wire)?;
    // nonce 在验签前即被消费(失败也烧掉,不可重放)——上游同序。
    verify_attestation(
        identity,
        handshake.nonces(),
        &attestation,
        &handshake.binding,
        verified_at_ms,
    )?;

    let served = serve_one(
        framed.into_inner(),
        config,
        peer,
        peer_gate,
        |validated| async move {
            Ok(OutboundResponse::Typed(ExchangeResponse {
                envelope: Some(serve_validated_pipe(
                    control,
                    clock,
                    now_monotonic_ns,
                    validated.envelope(),
                )),
            }))
        },
    )
    .await;
    if let Err(error) = served {
        eprintln!("dev_server: windows authenticated serve: {error}");
    }
    Ok(())
}

/// Windows 版 `serve_validated` 镜像(上游 auth.rs 私有函数;命令级墙钟
/// 经 `AuthorityClock` 幂等读,correlation id 上界外的请求为类型化失败信封)。
#[cfg(windows)]
fn serve_validated_pipe<H, A>(
    control: &RecoverySystemControl<'_, H, A>,
    clock: &AuthorityClock,
    now_monotonic_ns: u64,
    request: &nlos_schema::sabi::v1::Envelope,
) -> nlos_schema::sabi::v1::Envelope
where
    H: RecoveryHealthSource,
    A: SystemControlAuthorizer,
{
    use nlos_clock::NowRequest;
    use nlos_schema::REQUEST_ID_BYTES;
    use nlos_schema::sabi::v1::envelope;
    use nlos_system_control::{SystemControlError, failure_envelope};

    // 上游 `command_wall_key`(pub 但住在 cfg(unix) 的 auth 模块)的逐字
    // 镜像:correlation id 即幂等键。
    let command_wall_key =
        |correlation_id: &[u8; REQUEST_ID_BYTES]| IdempotencyKey::from_bytes(*correlation_id);
    let bounded_correlation =
        |request: &nlos_schema::sabi::v1::Envelope| match request.common_context.as_ref() {
            Some(envelope::CommonContext::RequestContext(context))
                if context.correlation_id.len() == REQUEST_ID_BYTES =>
            {
                let mut correlation = [0; REQUEST_ID_BYTES];
                correlation.copy_from_slice(&context.correlation_id);
                Some(correlation)
            }
            _ => None,
        };
    let Some(correlation_id) = bounded_correlation(request) else {
        return failure_envelope(request, &SystemControlError::UnboundedCorrelation);
    };
    match clock.wall_now(NowRequest {
        idempotency_key: command_wall_key(&correlation_id),
    }) {
        Ok(decision) => {
            let wall_ms = i64::try_from(decision.reading().as_u64()).unwrap_or(i64::MAX);
            control.handle_for_ipc(request, now_monotonic_ns, wall_ms)
        }
        Err(_) => failure_envelope(request, &SystemControlError::ClockWallUnavailable),
    }
}

/// 上游 auth.rs 私有 `handshake_wall_key` 的逐字镜像(握手验证时间的
/// 幂等键派生;域分隔常量相同,保持时钟幂等域同源)。
#[cfg(windows)]
fn handshake_wall_key(nonce: &[u8; nlos_schema::HANDSHAKE_NONCE_BYTES]) -> IdempotencyKey {
    use sha2::{Digest, Sha256};

    const HANDSHAKE_WALL_DOMAIN: &[u8] = b"llmos/control-auth/handshake-wall/v1";
    let mut hasher = Sha256::new();
    hasher.update(HANDSHAKE_WALL_DOMAIN);
    hasher.update(nonce);
    let digest: [u8; 32] = hasher.finalize().into();
    let mut key = [0; 16];
    key.copy_from_slice(&digest[..16]);
    IdempotencyKey::from_bytes(key)
}

impl DevFixture {
    /// plain 入口派发助手(测试一致性比对用):Unix 委托上游
    /// `dispatch_over_socket`(与真实 CLI 同一字节路径);Windows 为其
    /// named pipe 等价镜像(transport 换 named pipe,handler/receipt 同构)。
    pub async fn dispatch_plain(
        &self,
        command: &nlos_system_control::control::ControlCommand,
        process: Option<&dyn nlos_system_control::control::ProcessInspector>,
        resource: Option<&dyn nlos_system_control::control::ResourceInspector>,
        application: Option<&dyn nlos_system_control::control::ApplicationInspector>,
    ) -> Result<nlos_system_control::control::ControlReceipt, crate::error::DesktopError> {
        #[cfg(unix)]
        {
            nlos_system_control::control::dispatch_over_socket(
                &self.endpoint_plain,
                command,
                process,
                resource,
                application,
            )
            .await
            .map_err(|error| crate::error::from_control_error(&error))
        }
        #[cfg(windows)]
        {
            use nlos_ipc::{LocalRpcClient, TransportConfig, windows};
            use nlos_schema::sabi::v1::ExchangeRequest;
            use nlos_system_control::control::build_request_envelope;

            let name = crate::transport::normalize_endpoint(&self.endpoint_plain.name)?;
            let request = build_request_envelope(command)
                .map_err(|error| crate::error::from_control_error(&error))?;
            let config = TransportConfig::default();
            let (stream, _peer) = windows::connect(&name, config).await.map_err(|error| {
                crate::error::DesktopError::ipc(format!("control transport: {error}"))
            })?;
            let response = LocalRpcClient::new(stream, config)
                .exchange_validated(ExchangeRequest {
                    envelope: Some(request),
                })
                .await
                .map_err(|error| {
                    crate::error::DesktopError::ipc(format!("control transport: {error}"))
                })?;
            nlos_system_control::control::ControlReceipt::compose(
                command,
                response.envelope(),
                process,
                resource,
                application,
            )
            .map_err(|error| crate::error::from_control_error(&error))
        }
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
            application_id: None,
            plan_revision: None,
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

/// 结清预留的确定性演示事实(预算/成本视图与一致性自检共用)。
struct SettledReservation {
    resource_root: PathBuf,
    reservation_id: [u8; 16],
    account_id: [u8; 16],
    upper_bound: u64,
    usage_high_water: u64,
    consumption_count: u32,
}

/// W32-D:在 `root/resource` 下装配一条完整的真实资源权威链
/// (driver → account → quote → reserve → activate → consume×2 → finalize),
/// 产出一条已结清(FINALIZED)的预留——`inspect_cost_receipt` 只对结清
/// 预留开放,且其事实此后不可变,是预算/成本视图与一致性自检的确定性
/// 演示数据。上界 100、两次消费(30/70)、高水位 70。
fn create_settled_reservation(root: &Path) -> Result<SettledReservation, FixtureError> {
    let fail = |reason: &str| FixtureError::Task(reason.to_owned());
    let resource_root = root.join("resource");
    let authority = ResourceAuthority::open(&resource_root).map_err(|_| fail("open resource"))?;
    let driver = authority
        .register_driver(RegisterDriverRequest {
            profile_digest: [0xB1; 32],
            idempotency_key: IdempotencyKey::from_bytes([0xB2; 16]),
            created_at_ms: 10_000,
        })
        .map_err(|_| fail("register_driver"))?
        .record();
    let account = authority
        .create_account(CreateAccountRequest {
            initial_credit: 1_000,
            idempotency_key: IdempotencyKey::from_bytes([0xB3; 16]),
            created_at_ms: 10_100,
        })
        .map_err(|_| fail("create_account"))?;
    let quote = authority
        .create_quote(CreateQuoteRequest {
            driver_id: driver.driver_id,
            driver_generation: driver.generation,
            driver_fencing_token: driver.fencing_token,
            operation_proposal_digest: [0xB4; 32],
            pricing_version: [0xB5; 32],
            upper_bound: 100,
            demand_capacity: ResourceDemand {
                cpu_shares: 4,
                memory_mib: 256,
                io_weight: 50,
            },
            valid_until_ms: 100_000,
            idempotency_key: IdempotencyKey::from_bytes([0xB6; 16]),
            created_at_ms: 10_200,
        })
        .map_err(|_| fail("create_quote"))?
        .record();
    let call_id = CallId::from_bytes([0xB7; 16]);
    let operation_id = OperationId::from_bytes([0xB8; 16]);
    let reservation = authority
        .reserve(ReserveRequest {
            account_id: account.account_id,
            quote_id: quote.quote_id,
            call_id,
            operation_id,
            idempotency_key: IdempotencyKey::from_bytes([0xB9; 16]),
            demand: ResourceDemand {
                cpu_shares: 2,
                memory_mib: 128,
                io_weight: 25,
            },
            reserved_at_ms: 10_300,
        })
        .map_err(|_| fail("reserve"))?
        .record();
    let ActivationDecision::Activated(activation) = authority
        .activate(ActivateReservationRequest {
            reservation_id: reservation.reservation_id,
            call_id,
            operation_id,
            driver_id: driver.driver_id,
            driver_generation: driver.generation,
            driver_fencing_token: driver.fencing_token,
            activation_token: reservation.activation_token,
            activated_at_ms: 10_400,
        })
        .map_err(|_| fail("activate"))?
    else {
        return Err(fail("fresh activation"));
    };
    for (sequence, cumulative_usage, consumed_at_ms) in [(1, 30, 10_500u64), (2, 70, 10_600u64)] {
        authority
            .consume(ConsumeReservationRequest {
                reservation_id: reservation.reservation_id,
                operation_id,
                activation_receipt_id: activation.receipt_id,
                sequence,
                cumulative_usage,
                consumed_at_ms,
            })
            .map_err(|_| fail("consume"))?;
    }
    let FinalizeDecision::Finalized(finalization) = authority
        .finalize_reservation(FinalizeReservationRequest {
            reservation_id: reservation.reservation_id,
            operation_id,
            activation_receipt_id: activation.receipt_id,
            effect_closed_proof_digest: [0xBA; 32],
            final_seq: 2,
            final_usage: 70,
            finalized_at_ms: 10_700,
        })
        .map_err(|_| fail("finalize"))?
    else {
        return Err(fail("fresh finalization"));
    };
    Ok(SettledReservation {
        resource_root,
        reservation_id: *reservation.reservation_id.as_bytes(),
        account_id: *account.account_id.as_bytes(),
        upper_bound: reservation.upper_bound,
        usage_high_water: finalization.high_water,
        consumption_count: 2,
    })
}

/// 已结清预留的确定性成本事实(集成测试断言用)。
pub struct ReservationFacts {
    pub reservation_id_hex: String,
    pub account_id_hex: String,
    pub upper_bound: u64,
    pub usage_high_water: u64,
    pub consumption_count: u32,
}

/// 一次性开发夹具:双入口(认证/plain)服务 + 客户端接线参数。
pub struct DevFixture {
    /// keepalive:TempRoot 只为借用其 Drop(夹具释放时清理临时目录)。
    #[allow(dead_code)]
    root: TempRoot,
    endpoint_authenticated: FixtureEndpoint,
    endpoint_plain: FixtureEndpoint,
    tasks: Arc<SqliteTaskAuthority>,
    identity: Arc<IdentityAuthority>,
    clock: Arc<AuthorityClock>,
    handshake: Arc<HandshakeServerContext>,
    health: StubHealth,
    listener_authenticated: Option<ListenerAdapter>,
    listener_plain: Option<ListenerAdapter>,
    random: RandomSource,
    /// Windows OS 凭据门(F3-c3):named pipe 双入口共用的同用户 peer 门。
    #[cfg(windows)]
    peer_gate: nlos_ipc::windows::SameUserAuthorizer,
    principal_hex: String,
    key_seed_hex: String,
    plan_id_hex: String,
    resource_root: PathBuf,
    reservation_id_hex: String,
    account_id_hex: String,
    /// 结清预留的确定性成本事实(上界 100、两次消费、高水位 70)。
    settled_upper_bound: u64,
    settled_usage_high_water: u64,
    settled_consumption_count: u32,
}

impl DevFixture {
    /// 装配全部真实权威并绑定两个本地 IPC 端点(Unix socket / Windows
    /// named pipe;label 仅用于端点命名,≤3 字符)。
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
            resource_durable_retrying: 0,
            resource_durable_escalated: 0,
            resource_durable_unacknowledged_escalated: 0,
            resource_durable_resolved: 0,
            resource_consecutive_failed_cycles: 0,
            resource_total_inspected: 0,
            resource_total_finalized: 0,
            resource_domain_faulted: false,
        });

        let endpoint_authenticated = FixtureEndpoint::new(&format!("{label}a"));
        let endpoint_plain = FixtureEndpoint::new(&format!("{label}p"));
        #[cfg(unix)]
        let handshake = Arc::new(
            ServerHandshakeContext::new(endpoint_authenticated.as_ref(), 64)
                .map_err(FixtureError::HandshakeContext)?,
        );
        #[cfg(windows)]
        let handshake = Arc::new(
            WindowsHandshakeContext::new(&endpoint_authenticated.to_endpoint_string(), 64)
                .map_err(FixtureError::HandshakeContext)?,
        );
        let listener_authenticated = Some(bind_endpoint(&endpoint_authenticated)?);
        let listener_plain = Some(bind_endpoint(&endpoint_plain)?);
        let settled = create_settled_reservation(&root.0)?;
        #[cfg(windows)]
        let peer_gate = nlos_ipc::windows::SameUserAuthorizer::new().map_err(FixtureError::Io)?;

        Ok(Self {
            root,
            endpoint_authenticated,
            endpoint_plain,
            tasks,
            identity: Arc::new(identity),
            clock: Arc::new(clock),
            handshake,
            health,
            listener_authenticated,
            listener_plain,
            random,
            #[cfg(windows)]
            peer_gate,
            principal_hex: hex(binding.principal_id.as_bytes()),
            key_seed_hex: hex(&key_seed),
            plan_id_hex: hex(plan_id.as_bytes()),
            resource_root: settled.resource_root,
            reservation_id_hex: hex(&settled.reservation_id),
            account_id_hex: hex(&settled.account_id),
            settled_upper_bound: settled.upper_bound,
            settled_usage_high_water: settled.usage_high_water,
            settled_consumption_count: settled.consumption_count,
        })
    }

    /// 认证端点地址(GUI 会话配置 `LLMOS_DESKTOP_SOCKET` 的取值形态;
    /// Unix 为 socket 路径,Windows 为 `\\.\pipe\` 管道名)。
    #[must_use]
    pub fn endpoint_authenticated(&self) -> String {
        self.endpoint_authenticated.to_endpoint_string()
    }

    /// plain 端点地址(一致性比对入口;`dispatch_plain` 的服务端)。
    #[must_use]
    pub fn endpoint_plain(&self) -> String {
        self.endpoint_plain.to_endpoint_string()
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

    /// W32-D:本地资源权威根目录(预算/成本视图的 resource_root 接线值)。
    #[must_use]
    pub fn resource_root(&self) -> &Path {
        &self.resource_root
    }

    /// W32-D:已结清预留 id(32 hex)与其确定性成本事实。
    #[must_use]
    pub fn reservation_facts(&self) -> ReservationFacts {
        ReservationFacts {
            reservation_id_hex: self.reservation_id_hex.clone(),
            account_id_hex: self.account_id_hex.clone(),
            upper_bound: self.settled_upper_bound,
            usage_high_water: self.settled_usage_high_water,
            consumption_count: self.settled_consumption_count,
        }
    }

    /// 把 Ed25519 种子写入权限收窄的密钥文件(客户端派发时读取)。
    /// Unix:0600 权限位;Windows:owner-only DACL(`crate::winsec`,
    /// 0600 等价物——`key_file_acl_side.rs` 断言其单 ACE 结构)。
    pub fn write_key_file(&self, path: &Path) -> Result<(), FixtureError> {
        use std::io::Write;

        let mut file = open_key_file(path)?;
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
        #[cfg(windows)]
        let peer_gate = self.peer_gate.clone();
        tokio::spawn(async move {
            // Windows 的 named pipe accept 需要 &mut(Unix 不需要)。
            #[allow(unused_mut)]
            let Some(mut listener) = listener else {
                return;
            };
            loop {
                let control =
                    RecoverySystemControl::new(tasks.as_ref(), &health, &CapabilityPolicy);
                #[cfg(unix)]
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
                #[cfg(windows)]
                let outcome = authenticated_serve_one_control_pipe(
                    &mut listener,
                    TransportConfig::default(),
                    &control,
                    identity.as_ref(),
                    clock.as_ref(),
                    handshake.as_ref(),
                    &peer_gate,
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
        #[cfg(windows)]
        let peer_gate = self.peer_gate.clone();
        tokio::spawn(async move {
            #[allow(unused_mut)]
            let Some(mut listener) = listener else {
                return;
            };
            loop {
                #[cfg(unix)]
                let (stream, peer) = match listener.accept(TransportConfig::default()).await {
                    Ok(connection) => connection,
                    Err(nlos_ipc::IpcError::Timeout(nlos_ipc::IoOperation::Accept)) => continue,
                    Err(error) => {
                        eprintln!("dev_server: plain accept: {error}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                #[cfg(windows)]
                let (stream, peer) =
                    match accept_pipe(&mut listener, TransportConfig::default()).await {
                        Ok(connection) => connection,
                        Err(nlos_ipc::IpcError::Timeout(nlos_ipc::IoOperation::Accept)) => {
                            continue;
                        }
                        Err(error) => {
                            eprintln!("dev_server: plain accept: {error}");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                    };
                let wall_ms = i64::try_from(wall_now_ms()).unwrap_or(i64::MAX);
                let handler = {
                    let tasks = Arc::clone(&tasks);
                    let health = health.clone();
                    move |validated: nlos_schema::ValidatedExchangeRequest| {
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
                };
                // 一个坏交换不拖垮端点:handler 失败只影响该连接。
                #[cfg(unix)]
                let _ = serve_one(
                    stream,
                    TransportConfig::default(),
                    peer,
                    &AllowPeer,
                    handler,
                )
                .await;
                #[cfg(windows)]
                let _ = serve_one(
                    stream,
                    TransportConfig::default(),
                    peer,
                    &peer_gate,
                    handler,
                )
                .await;
            }
        })
    }
}
