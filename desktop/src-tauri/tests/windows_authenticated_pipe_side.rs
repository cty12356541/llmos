//! W52 Windows 端到端证据:desktop 的认证派发核心(`dispatch_control`:
//! 密钥文件读取 + ADR-0011 challenge-response + 回执投影)跨真实命名管道
//! 完成一次 `InspectHealth` 往返并断言回执。
//!
//! 不经 devfixture——其 harness 硬编码 Unix socket 路径与 `/dev/urandom`,
//! 属后续车道;本测试参照
//! `crates/nlos-system-control/tests/windows_named_pipe.rs` 的范式自建最小
//! 服务端:真 `IdentityAuthority`(Ed25519 验签)+ 真 `AuthorityClock`
//! (durable wall)+ 真 `SqliteTaskAuthority` + SC 跨平台认证服务入口
//! `authenticated_serve_one_control`(over `NamedPipeListenerAdapter`)。
//! 握手、交换、回执字节与 Unix 侧同源(nlos-ipc 平台分派包装 + 泛型核心),
//! 本测试只证 Windows 管道传输下的真实往返。

#![cfg(all(windows, feature = "dev-fixture"))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ed25519_dalek::SigningKey;
use nlos_clock::AuthorityClock;
use nlos_commit_coordinator::{RecoveryWorkerHealth, RecoveryWorkerState};
use nlos_identity::{BootstrapDecision, BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_ipc::handshake::transport::ServerHandshakeContext;
use nlos_ipc::windows::NamedPipeListenerAdapter;
use nlos_ipc::{PeerAuthorizer, PeerIdentity, TransportConfig};
use nlos_schema::sabi::v1::{
    ControlCommand as SabiWireCommand, GetSystemControlRequest, SabiRequestContext,
};
use nlos_system_control::auth::authenticated_serve_one_control;
use nlos_system_control::control::{
    CONTROL_CAPABILITY_GENERATION, CONTROL_CAPABILITY_SLOT, ControlCommand,
};
use nlos_system_control::{RecoveryHealthSource, RecoverySystemControl, SystemControlAuthorizer};
use nlos_task::SqliteTaskAuthority;
use nlos_types::IdempotencyKey;

use llmos_desktop_lib::dto::OutcomeDto;
use llmos_desktop_lib::ipc::dispatch_control;

static NEXT_PIPE: AtomicU64 = AtomicU64::new(0);

/// 临时状态根(测试退出时尽力清理)。
struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "llmosdt-win-auth-{}-{}",
            std::process::id(),
            NEXT_PIPE.fetch_add(1, Ordering::Relaxed),
        )))
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

/// 与 devfixture/测试 harness 同形的 capability 策略:只认控制面前缀的
/// 固定 handle 槽位。
struct CapabilityPolicy;

impl SystemControlAuthorizer for CapabilityPolicy {
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

/// 认证入口的 in-transport 对端门(能力策略仍是边界;同 daemon/devfixture)。
struct AllowPeer;

impl PeerAuthorizer for AllowPeer {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone)]
struct StubHealth(RecoveryWorkerHealth);

impl RecoveryHealthSource for StubHealth {
    fn recovery_health(&self) -> RecoveryWorkerHealth {
        self.0.clone()
    }
}

/// 一次性握手 nonce:确定性计数器(测试注入,镜像 SC 测试范式)。
fn next_nonce(counter: &Arc<AtomicU64>) -> [u8; 32] {
    let value = counter.fetch_add(1, Ordering::Relaxed);
    let mut nonce = [0u8; 32];
    nonce[..8].copy_from_slice(&value.to_be_bytes());
    nonce
}

#[tokio::test]
async fn authenticated_inspect_health_crosses_real_windows_named_pipe() {
    // 确定性 Ed25519 种子:客户端密钥文件与服务端 principal 自举共用。
    let seed = [0x51u8; 32];
    let root = TestRoot::new();
    let identity = IdentityAuthority::open(root.0.join("identity")).unwrap();
    let public_key = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
    let BootstrapDecision::Created(binding) = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [0xA1; 32],
            control_domain_policy_digest: [0xA2; 32],
            public_key,
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 0,
            // 测试有效期上限 2100-01-01(非机密)。
            key_valid_until_ms: 4_102_444_800_000,
            idempotency_key: IdempotencyKey::from_bytes([0xA3; 16]),
            created_at_ms: 0,
        })
        .unwrap()
    else {
        panic!("fresh authority bootstraps");
    };

    let clock = AuthorityClock::open(root.0.join("clock")).unwrap();
    let tasks = Arc::new(SqliteTaskAuthority::open(root.0.join("tasks.sqlite3")).unwrap());
    let health = StubHealth(RecoveryWorkerHealth {
        state: RecoveryWorkerState::BackingOff,
        completed_cycles: 7,
        ..RecoveryWorkerHealth::default()
    });

    let pipe_name = format!(
        r"\\.\pipe\llmos-desktop-auth-{}-{}",
        std::process::id(),
        NEXT_PIPE.fetch_add(1, Ordering::Relaxed),
    );
    let handshake = Arc::new(ServerHandshakeContext::new(Path::new(&pipe_name), 8).unwrap());
    let mut listener =
        NamedPipeListenerAdapter::bind(&pipe_name, 4, TransportConfig::default()).unwrap();

    let identity = Arc::new(identity);
    let nonce_counter = Arc::new(AtomicU64::new(0));
    let server = tokio::spawn(async move {
        let policy = CapabilityPolicy;
        let gate = AllowPeer;
        loop {
            let control = RecoverySystemControl::new(tasks.as_ref(), &health, &policy);
            let nonce = next_nonce(&nonce_counter);
            // 空闲 accept 超时与握手失败都是正常轮次,端点继续服务。
            let _ = authenticated_serve_one_control(
                &mut listener,
                TransportConfig::default(),
                &control,
                identity.as_ref(),
                &clock,
                handshake.as_ref(),
                &gate,
                10,
                move || nonce,
            )
            .await;
        }
    });

    // 客户端密钥文件:64 hex Ed25519 种子(desktop 派发时读取;Windows 侧
    // 无 Unix 0600 权限位语义,内容契约一致)。
    let key_file = root.0.join("operator.key");
    std::fs::write(&key_file, hex(&seed)).unwrap();
    let principal_hex = hex(binding.principal_id.as_bytes());

    let receipt = dispatch_control(
        &pipe_name,
        &principal_hex,
        key_file.to_str().unwrap_or_default(),
        ControlCommand::InspectHealth,
    )
    .await
    .expect("authenticated dispatch over named pipe");

    assert!(!receipt.receipt_hex.is_empty());
    match &receipt.outcome {
        OutcomeDto::Inspected {
            worker_state,
            completed_cycles,
            alerts,
            ..
        } => {
            assert_eq!(worker_state, "BackingOff");
            assert_eq!(*completed_cycles, 7);
            assert!(alerts.is_empty());
        }
        other => panic!("expected Inspected outcome, got {other:?}"),
    }

    server.abort();
}
