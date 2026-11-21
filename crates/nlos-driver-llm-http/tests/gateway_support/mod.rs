use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nlos_capability::CapabilityHandle;
use nlos_capability::service_call::{
    AuthorizationError, CallBinding, PolicyGrant, RuntimeCaller, ServiceAuthorizationBinding,
    ServiceAuthorizer, ServiceTime,
};
use nlos_driver_llm_http::llm_gateway::{
    GatewayConfig, GatewayLimits, ModelAccounting, ModelPrice, PriceRegistry, RootAccountBinding,
};
use nlos_driver_llm_http::{
    DirectKeyPoolConfig, DirectKeyPoolScheduler, DirectKeySlotConfig, HttpClient,
    KeySelectionStrategy, LlmEnforcementShim, SecretCredential,
};
use nlos_schema::sabi::v1::{LlmChatRequest, LlmChatTurn, LlmUsageWire, SchemaIdentity};
use nlos_schema::{SABI_ENVELOPE_SCHEMA, llm_gateway_schema_identity};
use nlos_types::{
    ApplicationId, CapabilityId, Generation, IdempotencyKey, NamespaceId, PrincipalId, ProcessId,
    SessionId,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub struct Policy(pub AtomicBool);
impl ServiceAuthorizer for Policy {
    fn authorize(
        &self,
        input: &ServiceAuthorizationBinding,
        _: ServiceTime,
    ) -> Result<PolicyGrant, AuthorizationError> {
        if !self.0.load(Ordering::SeqCst) {
            return Err(AuthorizationError::Denied);
        }
        Ok(PolicyGrant {
            binding: input.clone(),
            capability: input.call.capabilities[0],
            valid_from_ms: 1,
            valid_until_ms: 95_000,
        })
    }
}

static DIRECTORY_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
pub struct TestRoot(pub PathBuf);
impl TestRoot {
    pub fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let nonce = DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("nlos-b2-{}-{nanos}-{nonce}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn call(raw_key: u8, session: u8) -> CallBinding {
    CallBinding {
        caller: RuntimeCaller {
            principal: PrincipalId::from_bytes([1; 16]),
            application: ApplicationId::from_bytes([2; 16]),
            process: ProcessId::from_bytes([3; 16]),
            process_generation: Generation::INITIAL,
            session: SessionId::from_bytes([session; 16]),
        },
        idempotency_key: Some(IdempotencyKey::from_bytes([raw_key; 16])),
        deadline_monotonic_ns: Some(10_000_000_000),
        capabilities: vec![CapabilityHandle {
            capability_id: CapabilityId::from_bytes([5; 16]),
            generation: Generation::new(NonZeroU64::MIN),
        }],
    }
}

pub fn root() -> RootAccountBinding {
    RootAccountBinding {
        principal: call(1, 1).caller.principal,
        application: call(1, 1).caller.application,
        namespace: NamespaceId::from_bytes([6; 16]),
        authorization_domain: [7; 16],
        generation: Generation::INITIAL,
        valid_until_ms: 100_000,
    }
}

pub fn budget(amount: u64) -> LlmUsageWire {
    LlmUsageWire {
        api_calls: amount,
        prompt_tokens: amount,
        completion_tokens: amount,
        total_tokens: amount,
        cost_microcredits: amount,
    }
}

pub fn config(
    port: u16,
    policy: Option<Arc<dyn ServiceAuthorizer>>,
    clock_ms: Arc<AtomicU64>,
) -> GatewayConfig {
    let slots = vec![DirectKeySlotConfig::new(
        "synthetic-slot",
        "test",
        SecretCredential::new("synthetic-credential"),
    )];
    let pool = DirectKeyPoolScheduler::new(DirectKeyPoolConfig::new(
        format!("http://127.0.0.1:{port}/v1"),
        KeySelectionStrategy::RoundRobin,
        slots,
    ))
    .unwrap();
    let shim = LlmEnforcementShim::from_direct_pool(
        Arc::new(pool),
        "unit-model",
        "unit-model",
        Vec::new(),
    )
    .with_http_client(HttpClient::with_timeout(Duration::from_secs(2)));
    let price = ModelPrice {
        model: "unit-model".into(),
        version: 42,
        max_output_tokens: 64,
        accounting: ModelAccounting::InclusiveReasoning,
        prompt_microcredits_per_kilotoken: 250,
        output_microcredits_per_kilotoken: 750,
    };
    GatewayConfig {
        shim,
        prices: PriceRegistry::new([price]).unwrap(),
        limits: GatewayLimits {
            advisory_rows_per_window: 1,
            ..GatewayLimits::default()
        },
        authorizer: policy,
        time: Arc::new(move || {
            let wall_ms = clock_ms.load(Ordering::SeqCst);
            if wall_ms == 0 {
                return Err(AuthorizationError::TimeUnavailable);
            }
            Ok(ServiceTime {
                wall_ms,
                monotonic_ns: 100,
            })
        }),
    }
}

pub fn chat_request() -> LlmChatRequest {
    LlmChatRequest {
        schema: Some(llm_gateway_schema_identity()),
        model: "unit-model".into(),
        turns: vec![LlmChatTurn {
            role: "user".into(),
            content: "汉字🦀".into(),
        }],
        max_output_tokens: 12,
        temperature_milli: 100,
        session_hint: "arbitrary-hint".into(),
    }
}

pub async fn provider() -> (u16, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&hits);
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            seen.fetch_add(1, Ordering::SeqCst);
            let mut body = Vec::new();
            let mut chunk = [0; 4096];
            loop {
                let length = stream.read(&mut chunk).await.unwrap();
                if length == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..length]);
                if let Some(end) = body.windows(4).position(|window| window == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&body[..end]);
                    let expected: usize = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().unwrap())
                        })
                        .unwrap();
                    if body.len() >= end + 4 + expected {
                        break;
                    }
                }
            }
            let reply = r#"{"model":"unit-model","choices":[{"message":{"content":"答","reasoning_content":"算"}}],"usage":{"prompt_tokens":5,"completion_tokens":4,"completion_tokens_details":{"reasoning_tokens":3},"total_tokens":9}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (port, hits, task)
}

pub const VALID_BODY: &str = r#"{"model":"unit-model","choices":[{"message":{"content":"答","reasoning_content":"算"}}],"usage":{"prompt_tokens":5,"completion_tokens":4,"completion_tokens_details":{"reasoning_tokens":3},"total_tokens":9}}"#;

pub struct Reply {
    pub bytes: Option<Vec<u8>>,
    pub permit: Option<Arc<tokio::sync::Semaphore>>,
}

impl Reply {
    pub fn response(status: u16, body: &str) -> Self {
        let bytes = format!(
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        Self {
            bytes: Some(bytes.into_bytes()),
            permit: None,
        }
    }

    pub fn held(mut self) -> (Self, Arc<tokio::sync::Semaphore>) {
        let permit = Arc::new(tokio::sync::Semaphore::new(0));
        self.permit = Some(permit.clone());
        (self, permit)
    }
}

pub async fn endpoint(
    replies: Vec<Reply>,
) -> (
    u16,
    tokio::sync::mpsc::Receiver<usize>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let server = tokio::spawn(async move {
        let mut replies = replies.into_iter().enumerate();
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (index, reply) = replies.next().unwrap_or((
                usize::MAX,
                Reply {
                    bytes: None,
                    permit: None,
                },
            ));
            let tx = tx.clone();
            connections.spawn(async move {
                let mut data = vec![0; 128 * 1024];
                let mut received = 0;
                while received < data.len() {
                    let Ok(count) = socket.read(&mut data[received..]).await else {
                        return;
                    };
                    if count == 0 {
                        return;
                    }
                    received += count;
                    if let Some(end) = data[..received]
                        .windows(4)
                        .position(|part| part == b"\r\n\r\n")
                    {
                        let header = String::from_utf8_lossy(&data[..end]);
                        let length: usize = header
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().unwrap())
                            })
                            .unwrap();
                        if received >= end + 4 + length {
                            break;
                        }
                    }
                }
                tx.send(index).await.unwrap();
                if let Some(permit) = reply.permit {
                    permit.acquire().await.unwrap().forget();
                }
                if let Some(bytes) = reply.bytes {
                    let _ = socket.write_all(&bytes).await;
                }
            });
        }
    });
    (port, rx, server)
}

pub fn envelope_schema_identity() -> SchemaIdentity {
    SchemaIdentity {
        name: SABI_ENVELOPE_SCHEMA.to_owned(),
        major: 1,
        minor: 1,
        critical_extension_ids: Vec::new(),
        non_critical_extension_ids: Vec::new(),
    }
}
