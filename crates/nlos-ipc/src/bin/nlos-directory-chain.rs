use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use nlos_ipc::{
    FramedIo, IpcError, OutboundResponse, PeerAuthorizer, PeerIdentity, TransportConfig, serve_one,
};
use nlos_operation::{CompletionOutcome, OperationSpec, OperationState};
use nlos_runtime::FiberHandle;
use nlos_schema::sabi::v1::{
    DirectoryError, DirectoryErrorCode, ExchangeResponse, LocalEndpoint, LocalTransportKind,
    NegotiateServiceResponse, OperationReference, ReceiptReference, RetryDirective, SabiErrorCode,
    SabiFailure, SabiResponseContext, ServiceCandidate, ServiceVersion, envelope,
    negotiate_service_response,
};
use nlos_schema::{
    MethodSemantics, SABI_ENVELOPE_SCHEMA, decode_exchange_request,
    decode_negotiate_service_request, encode_negotiate_service_response,
    service_directory_schema_identity, validate_sabi_request_context,
};
use nlos_service_directory::{ServiceRegistration, SnapshotDirectory};
use nlos_store::{
    DurableCallResult, IdempotencyDecision, IdempotencyScope, OutboxKind, SqliteOperationStore,
    StoreError,
};
use nlos_types::{
    ApplicationId, CallbackId, CancellationScopeId, ExecutionFiberId, Generation, IdempotencyKey,
    OperationId, ReceiptId,
};
use sha2::{Digest, Sha256};

const DIRECTORY_SERVICE: &str = "service_directory";
const NEGOTIATE_METHOD: &str = "negotiate";
const BUSINESS_SERVICE: &str = "operation";
const ADMISSION_NOW_MONOTONIC_NS: u64 = 123_455;
const QUEUE_CHECK_NOW_MONOTONIC_NS: u64 = 123_456;
const RECOVERY_BUSINESS_EXCHANGES: usize = 8;

#[derive(Clone, Copy)]
enum ServerPhase {
    Commit,
    Recover,
}

impl ServerPhase {
    fn parse(value: &OsString) -> Result<Self, Box<dyn Error>> {
        match value.to_str() {
            Some("commit") => Ok(Self::Commit),
            Some("recover") => Ok(Self::Recover),
            _ => Err("server phase must be commit or recover".into()),
        }
    }
}

struct AllowConformancePeer;

impl PeerAuthorizer for AllowConformancePeer {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let directory_endpoint = arguments.next().ok_or("missing directory endpoint")?;
    let business_endpoint = arguments.next().ok_or("missing business endpoint")?;
    let authority_path = arguments.next().ok_or("missing authority database path")?;
    let phase = arguments.next().ok_or("missing server phase")?;
    let phase = ServerPhase::parse(&phase)?;
    if arguments.next().is_some() {
        return Err("unexpected extra argument".into());
    }
    run(directory_endpoint, business_endpoint, authority_path, phase).await
}

fn announce_ready() -> io::Result<()> {
    println!("READY");
    io::stdout().flush()
}

fn directory(
    transport: LocalTransportKind,
    business_endpoint: String,
) -> Result<SnapshotDirectory, Box<dyn Error>> {
    Ok(SnapshotDirectory::new([ServiceRegistration {
        candidate: ServiceCandidate {
            binding_id: vec![0x44; 16],
            generation: 7,
            service: BUSINESS_SERVICE.to_owned(),
            version: Some(ServiceVersion {
                schema_name: SABI_ENVELOPE_SCHEMA.to_owned(),
                major: 1,
                minor: 1,
            }),
            feature_ids: Vec::new(),
            transport_kinds: vec![transport.into()],
        },
        endpoint: LocalEndpoint {
            kind: transport.into(),
            address: business_endpoint,
        },
    }])?)
}

#[cfg(unix)]
async fn run(
    directory_endpoint: OsString,
    business_endpoint: OsString,
    authority_path: OsString,
    phase: ServerPhase,
) -> Result<(), Box<dyn Error>> {
    use nlos_ipc::unix::UnixListenerAdapter;

    struct EndpointGuard(Vec<PathBuf>);

    impl Drop for EndpointGuard {
        fn drop(&mut self) {
            for path in &self.0 {
                let _ = fs::remove_file(path);
            }
        }
    }

    let directory_path = PathBuf::from(directory_endpoint);
    let business_path = PathBuf::from(business_endpoint);
    let authority_path = PathBuf::from(authority_path);
    let business_address = business_path
        .to_str()
        .ok_or("business endpoint must be UTF-8")?
        .to_owned();
    let directory_listener = UnixListenerAdapter::bind(&directory_path)?;
    let business_listener = UnixListenerAdapter::bind(&business_path)?;
    let _guard = EndpointGuard(vec![directory_path, business_path]);
    let _database_guard =
        matches!(phase, ServerPhase::Recover).then(|| DatabaseGuard::new(authority_path.clone()));
    let authority = BusinessAuthority::open(&authority_path)?;
    let snapshot = directory(LocalTransportKind::UnixSocket, business_address)?;
    announce_ready()?;

    let (directory_stream, directory_peer) = directory_listener
        .accept(TransportConfig::default())
        .await?;
    serve_directory(directory_stream, directory_peer, snapshot).await?;
    match phase {
        ServerPhase::Commit => {
            let (business_stream, business_peer) =
                business_listener.accept(TransportConfig::default()).await?;
            commit_then_drop_response(business_stream, business_peer, &authority).await?;
            authority.assert_cancel_dispatches(1)?;
        }
        ServerPhase::Recover => {
            for _ in 0..RECOVERY_BUSINESS_EXCHANGES {
                let (stream, peer) = business_listener.accept(TransportConfig::default()).await?;
                serve_business(stream, peer, &authority).await?;
            }
            authority.assert_cancel_dispatches(0)?;
            authority.assert_control_outbox()?;
        }
    }
    Ok(())
}

#[cfg(windows)]
async fn run(
    directory_endpoint: OsString,
    business_endpoint: OsString,
    authority_path: OsString,
    phase: ServerPhase,
) -> Result<(), Box<dyn Error>> {
    use nlos_ipc::windows::NamedPipeListenerAdapter;

    let business_address = business_endpoint
        .clone()
        .into_string()
        .map_err(|_| "business endpoint must be UTF-8")?;
    let mut directory_listener =
        NamedPipeListenerAdapter::bind(directory_endpoint, 2, TransportConfig::default())?;
    let mut business_listener =
        NamedPipeListenerAdapter::bind(business_endpoint, 2, TransportConfig::default())?;
    let authority_path = PathBuf::from(authority_path);
    let _database_guard =
        matches!(phase, ServerPhase::Recover).then(|| DatabaseGuard::new(authority_path.clone()));
    let authority = BusinessAuthority::open(&authority_path)?;
    let snapshot = directory(LocalTransportKind::WindowsNamedPipe, business_address)?;
    announce_ready()?;

    let (directory_stream, directory_peer) = directory_listener
        .accept(TransportConfig::default())
        .await?;
    serve_directory(directory_stream, directory_peer, snapshot).await?;
    match phase {
        ServerPhase::Commit => {
            let (business_stream, business_peer) =
                business_listener.accept(TransportConfig::default()).await?;
            commit_then_drop_response(business_stream, business_peer, &authority).await?;
            authority.assert_cancel_dispatches(1)?;
        }
        ServerPhase::Recover => {
            for _ in 0..RECOVERY_BUSINESS_EXCHANGES {
                let (stream, peer) = business_listener.accept(TransportConfig::default()).await?;
                serve_business(stream, peer, &authority).await?;
            }
            authority.assert_cancel_dispatches(0)?;
            authority.assert_control_outbox()?;
        }
    }
    Ok(())
}

struct DatabaseGuard {
    path: PathBuf,
}

impl DatabaseGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for DatabaseGuard {
    fn drop(&mut self) {
        for path in [
            self.path.clone(),
            suffix_path(&self.path, "-wal"),
            suffix_path(&self.path, "-shm"),
        ] {
            let _ = fs::remove_file(path);
        }
    }
}

fn suffix_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

struct BusinessAuthority {
    store: SqliteOperationStore,
    cancel_dispatches: AtomicUsize,
}

enum BusinessExecution {
    Completed(DurableCallResult),
    Pending,
}

impl BusinessAuthority {
    fn open(path: &Path) -> Result<Self, nlos_store::StoreError> {
        Ok(Self {
            store: SqliteOperationStore::open(path)?,
            cancel_dispatches: AtomicUsize::new(0),
        })
    }

    fn assert_cancel_dispatches(&self, expected: usize) -> Result<(), Box<dyn Error>> {
        let actual = self.cancel_dispatches.load(Ordering::SeqCst);
        if actual == expected {
            Ok(())
        } else {
            Err(format!("expected {expected} durable dispatches, observed {actual}").into())
        }
    }

    fn assert_control_outbox(&self) -> Result<(), Box<dyn Error>> {
        let entries = self.store.pending_outbox(16)?;
        let wake_count = entries
            .iter()
            .filter(|entry| entry.kind == OutboxKind::WakeFiber)
            .count();
        let reconcile_count = entries
            .iter()
            .filter(|entry| entry.kind == OutboxKind::ReconcileEffect)
            .count();
        let no_effect_count = entries
            .iter()
            .filter(|entry| matches!(entry.state, OperationState::CancelledBeforeEffect { .. }))
            .count();
        let partial_count = entries
            .iter()
            .filter(|entry| matches!(entry.state, OperationState::PartialEffect { .. }))
            .count();
        let unknown_count = entries
            .iter()
            .filter(|entry| matches!(entry.state, OperationState::EffectUnknown { .. }))
            .count();
        if entries.len() == 5
            && wake_count == 3
            && reconcile_count == 2
            && no_effect_count == 2
            && partial_count == 1
            && unknown_count == 1
        {
            Ok(())
        } else {
            Err(format!(
                "unexpected control outbox: total={}, wake={wake_count}, reconcile={reconcile_count}, no_effect={no_effect_count}, partial={partial_count}, unknown={unknown_count}",
                entries.len()
            )
            .into())
        }
    }
}

async fn serve_directory<S>(
    stream: S,
    peer: PeerIdentity,
    directory: SnapshotDirectory,
) -> Result<(), nlos_ipc::IpcError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    serve_one(
        stream,
        TransportConfig::default(),
        peer,
        &AllowConformancePeer,
        move |request| async move {
            let mut envelope = request.envelope().clone();
            let response =
                if envelope.service == DIRECTORY_SERVICE && envelope.method == NEGOTIATE_METHOD {
                    let negotiation = decode_negotiate_service_request(&envelope.payload)?;
                    directory.negotiate(&negotiation)
                } else {
                    NegotiateServiceResponse {
                        schema: Some(service_directory_schema_identity()),
                        result: Some(negotiate_service_response::Result::Error(DirectoryError {
                            code: DirectoryErrorCode::InvalidRequest.into(),
                            service: String::new(),
                        })),
                    }
                };
            envelope.payload = encode_negotiate_service_response(&response)?;
            Ok(OutboundResponse::Typed(ExchangeResponse {
                envelope: Some(envelope),
            }))
        },
    )
    .await
}

async fn commit_then_drop_response<S>(
    stream: S,
    peer: PeerIdentity,
    authority: &BusinessAuthority,
) -> Result<(), IpcError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    AllowConformancePeer
        .authorize(&peer)
        .map_err(IpcError::AuthorizationDenied)?;
    let mut connection = FramedIo::new(stream, TransportConfig::default());
    let wire = connection.receive().await?;
    let request = decode_exchange_request(&wire)?;
    let _committed_response = process_business(&request, authority)?;
    Ok(())
}

async fn serve_business<S>(
    stream: S,
    peer: PeerIdentity,
    authority: &BusinessAuthority,
) -> Result<(), IpcError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    serve_one(
        stream,
        TransportConfig::default(),
        peer,
        &AllowConformancePeer,
        |request| async move { process_business(&request, authority) },
    )
    .await
}

fn process_business(
    request: &nlos_schema::ValidatedExchangeRequest,
    authority: &BusinessAuthority,
) -> Result<OutboundResponse, IpcError> {
    let request_context = validate_sabi_request_context(
        request.envelope(),
        MethodSemantics::LONG_RUNNING_MUTATION,
        ADMISSION_NOW_MONOTONIC_NS,
    )?;
    let caller = request_context
        .caller
        .as_ref()
        .ok_or(IpcError::ServiceFailure("validated caller is missing"))?;
    let correlation_id = request_context.correlation_id.clone();
    let application_id = ApplicationId::from_bytes(fixed16(&caller.application_id)?);
    let idempotency_key = IdempotencyKey::from_bytes(fixed16(&request_context.idempotency_key)?);
    let scope = IdempotencyScope {
        application_id,
        service: request.envelope().service.clone(),
        method: request.envelope().method.clone(),
    };
    let request_digest = Sha256::digest(&request.envelope().payload).into();
    let operation_id = stable_operation_id(&scope, idempotency_key);
    let spec = OperationSpec {
        operation_id,
        generation: Generation::INITIAL,
        owner_fiber: FiberHandle {
            fiber_id: ExecutionFiberId::from_bytes(fixed16(&caller.process_id)?),
            generation: Generation::INITIAL,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes(idempotency_key.into_bytes()),
        cancellation_generation: Generation::INITIAL,
    };
    let decision = match authority.store.begin_idempotent_operation(
        &scope,
        idempotency_key,
        request_digest,
        spec,
    ) {
        Ok(decision) => decision,
        Err(StoreError::IdempotencyConflict) => {
            let original = authority
                .store
                .inspect_idempotent_operation(&scope, idempotency_key)
                .map_err(|_| IpcError::ServiceFailure("idempotency conflict lookup failed"))?
                .ok_or(IpcError::ServiceFailure(
                    "idempotency conflict lacks original operation",
                ))?;
            return Ok(conflict_response(
                request,
                correlation_id,
                original.operation(),
            ));
        }
        Err(_) => {
            return Err(IpcError::ServiceFailure("durable idempotency claim failed"));
        }
    };

    let result = match decision {
        IdempotencyDecision::Created(operation) => match execute_new_business_operation(
            request,
            authority,
            operation,
            request_context.deadline_monotonic_ns,
        )? {
            BusinessExecution::Completed(result) => result,
            BusinessExecution::Pending => {
                return Ok(uncertain_response(request, correlation_id, operation));
            }
        },
        IdempotencyDecision::Completed(result) => result,
        IdempotencyDecision::PendingOrUncertain(operation) => {
            return Ok(uncertain_response(request, correlation_id, operation));
        }
    };
    completed_business_response(request, authority, correlation_id, result)
}

fn execute_new_business_operation(
    request: &nlos_schema::ValidatedExchangeRequest,
    authority: &BusinessAuthority,
    operation: nlos_operation::OperationHandle,
    deadline_monotonic_ns: u64,
) -> Result<BusinessExecution, IpcError> {
    let method = request.envelope().method.as_str();
    if method == "deadline_before_dispatch" && deadline_monotonic_ns <= QUEUE_CHECK_NOW_MONOTONIC_NS
    {
        let result = authority
            .store
            .cancel_idempotent_before_dispatch(operation, ReceiptId::from_bytes([0xa1; 16]), &[])
            .map_err(|_| IpcError::ServiceFailure("durable pre-dispatch deadline failed"))?;
        return Ok(BusinessExecution::Completed(result));
    }
    if method == "cancel_before_dispatch" {
        let result = authority
            .store
            .cancel_idempotent_before_dispatch(operation, ReceiptId::from_bytes([0xa2; 16]), &[])
            .map_err(|_| IpcError::ServiceFailure("durable pre-dispatch cancellation failed"))?;
        return Ok(BusinessExecution::Completed(result));
    }
    if method == "cancel" {
        authority.cancel_dispatches.fetch_add(1, Ordering::SeqCst);
    }
    let ticket = authority
        .store
        .dispatch(operation, CallbackId::from_bytes([0x88; 16]))
        .map_err(|_| IpcError::ServiceFailure("durable dispatch failed"))?;
    if method == "pending" {
        return Ok(BusinessExecution::Pending);
    }

    let result = if method == "cancel_after_dispatch" {
        authority
            .store
            .request_cancel(operation, ReceiptId::from_bytes([0xa3; 16]))
            .map_err(|_| IpcError::ServiceFailure("durable cancel request failed"))?;
        authority
            .store
            .complete_idempotent_operation(
                ticket,
                CompletionOutcome::PartialEffect {
                    receipt_id: ReceiptId::from_bytes([0xa4; 16]),
                },
                &[],
            )
            .map_err(|_| IpcError::ServiceFailure("durable partial completion failed"))?
    } else if method == "deadline_after_dispatch"
        && deadline_monotonic_ns <= QUEUE_CHECK_NOW_MONOTONIC_NS
    {
        authority
            .store
            .request_cancel(operation, ReceiptId::from_bytes([0xa5; 16]))
            .map_err(|_| IpcError::ServiceFailure("durable deadline cancel failed"))?;
        authority
            .store
            .complete_idempotent_operation(
                ticket,
                CompletionOutcome::EffectUnknown {
                    receipt_id: ReceiptId::from_bytes([0xa6; 16]),
                },
                &[],
            )
            .map_err(|_| IpcError::ServiceFailure("durable effect-unknown completion failed"))?
    } else {
        let mut result_wire = request.envelope().payload.clone();
        result_wire.push(0xd0);
        authority
            .store
            .complete_idempotent_operation(
                ticket,
                CompletionOutcome::Completed {
                    receipt_id: ReceiptId::from_bytes([0x99; 16]),
                },
                &result_wire,
            )
            .map_err(|_| IpcError::ServiceFailure("durable completion failed"))?
    };
    Ok(BusinessExecution::Completed(result))
}

fn completed_business_response(
    request: &nlos_schema::ValidatedExchangeRequest,
    authority: &BusinessAuthority,
    correlation_id: Vec<u8>,
    result: DurableCallResult,
) -> Result<OutboundResponse, IpcError> {
    let state = authority
        .store
        .inspect(result.operation)
        .map_err(|_| IpcError::ServiceFailure("durable result state lookup failed"))?
        .state;
    let response = match state {
        OperationState::CancelledBeforeEffect { .. }
            if request.envelope().method == "deadline_before_dispatch" =>
        {
            terminal_failure_response(
                request,
                correlation_id,
                result,
                SabiErrorCode::Deadline,
                RetryDirective::DoNotRetry,
                "deadline expired before effect dispatch",
            )
        }
        OperationState::CancelledBeforeEffect { .. } => terminal_failure_response(
            request,
            correlation_id,
            result,
            SabiErrorCode::Cancelled,
            RetryDirective::DoNotRetry,
            "operation cancelled before effect dispatch",
        ),
        OperationState::PartialEffect { .. } => terminal_failure_response(
            request,
            correlation_id,
            result,
            SabiErrorCode::Partial,
            RetryDirective::DoNotRetry,
            "cancellation observed after a partial effect",
        ),
        OperationState::EffectUnknown { .. } => terminal_failure_response(
            request,
            correlation_id,
            result,
            SabiErrorCode::EffectUnknown,
            RetryDirective::QueryOperationOrRetrySameIdempotencyKey,
            "deadline expired after dispatch; effect is unknown",
        ),
        OperationState::Completed { .. } => success_response(request, correlation_id, result),
        OperationState::Failed { .. }
        | OperationState::Registered
        | OperationState::Dispatched
        | OperationState::CancelRequested => {
            return Err(IpcError::ServiceFailure(
                "durable result has incompatible operation state",
            ));
        }
    };
    Ok(response)
}

fn success_response(
    request: &nlos_schema::ValidatedExchangeRequest,
    correlation_id: Vec<u8>,
    result: DurableCallResult,
) -> OutboundResponse {
    let mut envelope = request.envelope().clone();
    envelope.payload = result.result_wire;
    envelope.common_context = Some(envelope::CommonContext::ResponseContext(
        SabiResponseContext {
            correlation_id,
            operation: Some(operation_reference(result.operation)),
            receipts: vec![ReceiptReference {
                receipt_id: result.receipt_id.into_bytes().to_vec(),
            }],
            failure: None,
        },
    ));
    OutboundResponse::Typed(ExchangeResponse {
        envelope: Some(envelope),
    })
}

fn terminal_failure_response(
    request: &nlos_schema::ValidatedExchangeRequest,
    correlation_id: Vec<u8>,
    result: DurableCallResult,
    code: SabiErrorCode,
    retry: RetryDirective,
    safe_message: &str,
) -> OutboundResponse {
    let mut envelope = request.envelope().clone();
    envelope.payload = result.result_wire;
    envelope.common_context = Some(envelope::CommonContext::ResponseContext(
        SabiResponseContext {
            correlation_id,
            operation: Some(operation_reference(result.operation)),
            receipts: vec![ReceiptReference {
                receipt_id: result.receipt_id.into_bytes().to_vec(),
            }],
            failure: Some(SabiFailure {
                code: code.into(),
                retry: retry.into(),
                safe_message: safe_message.to_owned(),
            }),
        },
    ));
    OutboundResponse::Typed(ExchangeResponse {
        envelope: Some(envelope),
    })
}

fn uncertain_response(
    request: &nlos_schema::ValidatedExchangeRequest,
    correlation_id: Vec<u8>,
    operation: nlos_operation::OperationHandle,
) -> OutboundResponse {
    let mut envelope = request.envelope().clone();
    envelope.payload.clear();
    envelope.common_context = Some(envelope::CommonContext::ResponseContext(
        SabiResponseContext {
            correlation_id,
            operation: Some(operation_reference(operation)),
            receipts: Vec::new(),
            failure: Some(SabiFailure {
                code: SabiErrorCode::Uncertain.into(),
                retry: RetryDirective::QueryOperationOrRetrySameIdempotencyKey.into(),
                safe_message: "operation result is not yet durable".to_owned(),
            }),
        },
    ));
    OutboundResponse::Typed(ExchangeResponse {
        envelope: Some(envelope),
    })
}

fn conflict_response(
    request: &nlos_schema::ValidatedExchangeRequest,
    correlation_id: Vec<u8>,
    operation: nlos_operation::OperationHandle,
) -> OutboundResponse {
    let mut envelope = request.envelope().clone();
    envelope.payload.clear();
    envelope.common_context = Some(envelope::CommonContext::ResponseContext(
        SabiResponseContext {
            correlation_id,
            operation: Some(operation_reference(operation)),
            receipts: Vec::new(),
            failure: Some(SabiFailure {
                code: SabiErrorCode::Conflict.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message: "idempotency key conflicts with the original request".to_owned(),
            }),
        },
    ));
    OutboundResponse::Typed(ExchangeResponse {
        envelope: Some(envelope),
    })
}

fn operation_reference(operation: nlos_operation::OperationHandle) -> OperationReference {
    OperationReference {
        operation_id: operation.operation_id.into_bytes().to_vec(),
        generation: operation.generation.get(),
    }
}

fn fixed16(bytes: &[u8]) -> Result<[u8; 16], IpcError> {
    bytes
        .try_into()
        .map_err(|_| IpcError::ServiceFailure("validated identifier length changed"))
}

fn stable_operation_id(scope: &IdempotencyScope, key: IdempotencyKey) -> OperationId {
    let mut digest = Sha256::new();
    digest.update(scope.application_id.as_bytes());
    digest.update((scope.service.len() as u64).to_be_bytes());
    digest.update(scope.service.as_bytes());
    digest.update((scope.method.len() as u64).to_be_bytes());
    digest.update(scope.method.as_bytes());
    digest.update(key.as_bytes());
    let digest = digest.finalize();
    let mut operation_id = [0_u8; 16];
    operation_id.copy_from_slice(&digest[..16]);
    OperationId::from_bytes(operation_id)
}
