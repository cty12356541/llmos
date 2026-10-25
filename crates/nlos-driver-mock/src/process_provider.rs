//! Out-of-process OS child provider driver (`OutOfProcessProviderDriver`) and
//! server-side `nlos.sabi.DriverProvider` SABI envelope handler (`#9 C-PROVIDER-REAL`).
//!
//! Spawns and supervises the standalone `nlos-provider-worker` OS process,
//! connects over cross-platform ADR-0011 authenticated IPC (`UnixStream` on
//! Unix, Windows Named Pipe via `nlos_ipc::windows` on Windows), exchanges
//! bounded `nlos.sabi.DriverProvider` v1.0 protobuf payloads, synchronizes
//! observational degradation/recovery with [`ProviderCache`], and enforces
//! `[TASK-PROVIDER-001]` (`PROVIDER_ISOLATION_UNVERIFIED` vs attested) on
//! returned execution receipts.

use std::error::Error;
use std::fmt;
use std::io::{BufRead, BufReader};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::Duration;

use nlos_ipc::handshake::HandshakeError;
use nlos_ipc::handshake::transport::authenticated_connect;
use nlos_ipc::{IpcError, LocalRpcClient, TransportConfig};
use nlos_operation::{OperationHandle, OperationSpec};
use nlos_schema::sabi::v1::{
    CallerIdentity, CapabilityHandle, Envelope, ExchangeRequest, OperationReference,
    ReceiptReference, RetryDirective, SabiErrorCode, SabiFailure, SabiRequestContext,
    SabiResponseContext, SchemaIdentity, envelope,
};
use nlos_schema::{
    CommonSemanticsError, CompatibilityError, HANDSHAKE_SIGNATURE_BYTES, MethodSemantics,
    REQUEST_ID_BYTES, SABI_ENVELOPE_SCHEMA, validate_sabi_request_context,
    validate_sabi_response_context,
};
use nlos_store::{CancelRequestDecision, StoreError};
use nlos_types::{CallbackId, Generation, IdempotencyKey, OperationId, PrincipalId};

use crate::cache::{CacheHealth, DegradationCause, ProviderCache};
use crate::proto_codec::{
    CANCEL_PROVIDER_METHOD, COMPLETE_PROVIDER_METHOD, CancelProviderOperation,
    CancelProviderOutcome, DISPATCH_PROVIDER_METHOD, DRIVER_PROVIDER_SERVICE,
    DecodedProviderRequest, DecodedProviderResponse, ProtoCodecError, ProviderExecutionReceipt,
    ProviderIsolationAttestation, ProviderIsolationPosture, ProviderIsolationProfile,
    QUERY_ISOLATION_PROVIDER_METHOD, REGISTER_PROVIDER_METHOD, decode_request_proto,
    decode_response_proto, derive_worker_compute_digest, encode_cancel_ack_proto,
    encode_cancel_request_proto, encode_complete_ack_proto, encode_complete_request_proto,
    encode_dispatch_ack_proto, encode_dispatch_request_proto, encode_isolation_report_proto,
    encode_query_isolation_request_proto, encode_register_ack_proto, encode_register_request_proto,
    terminal_receipt_of_state,
};
use crate::provider::{
    CompleteProviderOperation, DispatchProviderOperation, DispatchProviderOutcome, MockProvider,
    ProviderError, ProviderFaultMode, RegisterProviderOutcome,
};

const WORKER_READY_PREFIX: &str = "READY ";
const WORKER_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Signer callback used by [`OutOfProcessProviderDriver`] during the ADR-0011
/// connection-level principal handshake. Keeping signing behind a closure
/// avoids holding raw private key material inside the driver and composes with
/// both test keys and non-exportable custody vaults (`C-CUSTODY-001`).
pub type ProviderHandshakeSigner =
    dyn Fn(&[u8; 32]) -> Result<[u8; HANDSHAKE_SIGNATURE_BYTES], HandshakeError> + Send + Sync;

/// Spawn and endpoint parameters for one `nlos-provider-worker` OS process.
#[derive(Clone, Debug)]
pub struct ProviderWorkerConfig {
    pub worker_binary: PathBuf,
    pub store_root: PathBuf,
    pub identity_root: PathBuf,
    pub clock_root: PathBuf,
    pub endpoint: PathBuf,
    pub isolation_profile: ProviderIsolationProfile,
    pub attestation_digest_override: Option<[u8; 32]>,
    pub allowed_principal: Option<PrincipalId>,
    pub max_requests: Option<usize>,
}

/// Authenticated caller identity and signing closure presented by
/// [`OutOfProcessProviderDriver`] on every connection to `nlos-provider-worker`.
#[derive(Clone)]
pub struct ProviderClientCredentials {
    pub principal_id: PrincipalId,
    pub application_id: [u8; 16],
    pub process_id: [u8; 16],
    pub process_generation: NonZeroU64,
    pub capability_slot: u64,
    pub capability_generation: NonZeroU64,
    pub signer: Arc<ProviderHandshakeSigner>,
}

impl fmt::Debug for ProviderClientCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderClientCredentials")
            .field("principal_id", &self.principal_id)
            .field("application_id", &self.application_id)
            .field("process_id", &self.process_id)
            .field("process_generation", &self.process_generation)
            .field("capability_slot", &self.capability_slot)
            .field("capability_generation", &self.capability_generation)
            .finish_non_exhaustive()
    }
}

/// Client-side error surface of [`OutOfProcessProviderDriver`].
#[derive(Debug)]
pub enum OutOfProcessProviderError {
    /// Worker binary failed to spawn, exited before announcing `READY`, or
    /// could not be controlled via OS process primitives.
    WorkerProcess(String),
    /// The worker process or IPC endpoint was unreachable (`HostLost`, broken
    /// pipe, connection timeout, or dead worker). Triggers `ProviderCache`
    /// degradation (`CacheHealth::Degraded`).
    Unreachable(String),
    /// Local handshake failure prior to sending a request (for example invalid
    /// challenge schema or local signer error).
    Handshake(HandshakeError),
    /// Local or remote wire violation on the frozen `nlos.sabi.DriverProvider`
    /// channel or SABI response envelope.
    Wire(String),
    /// Worker computed an unexpected digest that disagreed with the
    /// deterministic `(operation, callback, seed)` contract.
    ComputeDigestMismatch,
    /// The authenticated worker answered with a bounded SABI rejection (for
    /// example `Conflict` on stale generation, `InvalidArgument` on malformed
    /// payload, or `Rights` on unauthorized principal).
    RemoteRejected {
        code: SabiErrorCode,
        retry: RetryDirective,
        safe_message: String,
    },
}

impl fmt::Display for OutOfProcessProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkerProcess(detail) => {
                write!(formatter, "provider worker process error: {detail}")
            }
            Self::Unreachable(detail) => {
                write!(formatter, "provider worker endpoint unreachable: {detail}")
            }
            Self::Handshake(error) => {
                write!(formatter, "provider handshake failed: {error}")
            }
            Self::Wire(detail) => write!(formatter, "provider wire contract violation: {detail}"),
            Self::ComputeDigestMismatch => {
                formatter.write_str("provider worker returned a mismatched compute digest")
            }
            Self::RemoteRejected {
                code,
                retry,
                safe_message,
            } => write!(
                formatter,
                "provider rejected request ({code:?}, {retry:?}): {safe_message}"
            ),
        }
    }
}

impl Error for OutOfProcessProviderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Handshake(error) => Some(error),
            Self::WorkerProcess(_)
            | Self::Unreachable(_)
            | Self::Wire(_)
            | Self::ComputeDigestMismatch
            | Self::RemoteRejected { .. } => None,
        }
    }
}

struct SupervisedWorker {
    child: Child,
    pid: u32,
}

/// Real out-of-process provider driver supervising `nlos-provider-worker` over
/// cross-platform ADR-0011 authenticated IPC and the frozen
/// `nlos.sabi.DriverProvider` protobuf schema.
pub struct OutOfProcessProviderDriver {
    worker_config: ProviderWorkerConfig,
    credentials: ProviderClientCredentials,
    transport_config: TransportConfig,
    worker: Mutex<Option<SupervisedWorker>>,
    health: Mutex<CacheHealth>,
    linked_cache: Option<Arc<ProviderCache>>,
    next_request_seq: AtomicU64,
}

impl OutOfProcessProviderDriver {
    /// Spawns `nlos-provider-worker` as a real OS child process and waits for
    /// its `READY endpoint=<...> pid=<os_pid>` readiness line before returning.
    ///
    /// # Errors
    ///
    /// Returns [`OutOfProcessProviderError::WorkerProcess`] if spawning fails
    /// or the child exits/times out before announcing `READY`.
    pub fn spawn(
        worker_config: ProviderWorkerConfig,
        credentials: ProviderClientCredentials,
        transport_config: TransportConfig,
    ) -> Result<Self, OutOfProcessProviderError> {
        let supervised = spawn_worker_process(&worker_config)?;
        Ok(Self {
            worker_config,
            credentials,
            transport_config,
            worker: Mutex::new(Some(supervised)),
            health: Mutex::new(CacheHealth::Healthy),
            linked_cache: None,
            next_request_seq: AtomicU64::new(1),
        })
    }

    /// Creates a client handle targeting an already-running
    /// `nlos-provider-worker` endpoint without spawning or owning a new OS
    /// child process.
    #[must_use]
    pub const fn connect_existing(
        worker_config: ProviderWorkerConfig,
        credentials: ProviderClientCredentials,
        transport_config: TransportConfig,
    ) -> Self {
        Self {
            worker_config,
            credentials,
            transport_config,
            worker: Mutex::new(None),
            health: Mutex::new(CacheHealth::Healthy),
            linked_cache: None,
            next_request_seq: AtomicU64::new(1),
        }
    }

    /// Links a shared [`ProviderCache`] so worker crashes and recoveries
    /// synchronously update `ProviderCache::health()` for all in-process
    /// consumers observing that cache entry.
    #[must_use]
    pub fn with_provider_cache(mut self, cache: Arc<ProviderCache>) -> Self {
        self.linked_cache = Some(cache);
        self
    }

    /// Endpoint path (`Unix` socket path or `Windows` named pipe path).
    #[must_use]
    pub fn endpoint(&self) -> &Path {
        &self.worker_config.endpoint
    }

    /// Last observed [`CacheHealth`] of the out-of-process worker.
    #[must_use]
    pub fn health(&self) -> CacheHealth {
        *self.lock_health()
    }

    /// Returns the OS PID of the currently running `nlos-provider-worker`
    /// child process, or `None` if it has exited or been killed.
    #[must_use]
    pub fn worker_pid(&self) -> Option<u32> {
        let mut guard = self.lock_worker();
        let entry = guard.as_mut()?;
        if let Ok(None) = entry.child.try_wait() {
            Some(entry.pid)
        } else {
            *guard = None;
            None
        }
    }

    /// Terminates the supervised `nlos-provider-worker` OS process (`kill` +
    /// `wait`) to simulate or enforce a hard worker crash/teardown.
    ///
    /// Observational health follows [`ProviderCache`] discipline: health is
    /// updated when the next consumer RPC observes the unreachable endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`OutOfProcessProviderError::WorkerProcess`] if waiting on the
    /// killed process fails.
    pub fn kill_worker(&self) -> Result<(), OutOfProcessProviderError> {
        let taken = self.lock_worker().take();
        if let Some(mut supervised) = taken {
            let _ = supervised.child.kill();
            supervised.child.wait().map_err(|error| {
                OutOfProcessProviderError::WorkerProcess(format!(
                    "waiting for killed worker pid {} failed: {error}",
                    supervised.pid
                ))
            })?;
        }
        Ok(())
    }

    /// Respawns a fresh `nlos-provider-worker` OS child process against the
    /// same durable authority roots and IPC endpoint, returning the new OS PID.
    ///
    /// # Errors
    ///
    /// Returns [`OutOfProcessProviderError::WorkerProcess`] if cleanup or
    /// startup fails.
    pub fn respawn_worker(&self) -> Result<u32, OutOfProcessProviderError> {
        self.kill_worker()?;
        let supervised = spawn_worker_process(&self.worker_config)?;
        let pid = supervised.pid;
        *self.lock_worker() = Some(supervised);
        Ok(pid)
    }

    /// Registers an [`OperationSpec`] idempotently on the out-of-process worker.
    ///
    /// # Errors
    ///
    /// Returns [`OutOfProcessProviderError`] on transport unreachability,
    /// handshake rejection, wire violation, or durable authority rejection.
    pub async fn register(
        &self,
        spec: OperationSpec,
        idempotency_key: IdempotencyKey,
    ) -> Result<RegisterProviderOutcome, OutOfProcessProviderError> {
        let payload = encode_register_request_proto(&spec)
            .map_err(|error| OutOfProcessProviderError::Wire(error.to_string()))?;
        let decoded = self
            .exchange_typed(
                REGISTER_PROVIDER_METHOD,
                MethodSemantics::MUTATION,
                payload,
                Some(idempotency_key),
            )
            .await?;
        match decoded {
            DecodedProviderResponse::RegisterAck(ack) => Ok(ack),
            _ => Err(OutOfProcessProviderError::Wire(
                ProtoCodecError::UnexpectedResponseArm.to_string(),
            )),
        }
    }

    /// Opens or replays the durable `prepare -> activate` dispatch boundary on
    /// the out-of-process worker.
    ///
    /// # Errors
    ///
    /// Returns [`OutOfProcessProviderError`] on transport unreachability,
    /// handshake rejection, wire violation, or durable authority rejection.
    pub async fn dispatch(
        &self,
        request: DispatchProviderOperation,
        idempotency_key: IdempotencyKey,
    ) -> Result<DispatchProviderOutcome, OutOfProcessProviderError> {
        let payload = encode_dispatch_request_proto(&request)
            .map_err(|error| OutOfProcessProviderError::Wire(error.to_string()))?;
        let decoded = self
            .exchange_typed(
                DISPATCH_PROVIDER_METHOD,
                MethodSemantics::MUTATION,
                payload,
                Some(idempotency_key),
            )
            .await?;
        match decoded {
            DecodedProviderResponse::DispatchAck(ack) => Ok(ack),
            _ => Err(OutOfProcessProviderError::Wire(
                ProtoCodecError::UnexpectedResponseArm.to_string(),
            )),
        }
    }

    /// Executes deterministic worker compute + durable completion on the
    /// out-of-process worker and enforces `[TASK-PROVIDER-001]` isolation
    /// posture on the returned [`ProviderExecutionReceipt`].
    ///
    /// # Errors
    ///
    /// Returns [`OutOfProcessProviderError`] on transport unreachability,
    /// handshake rejection, compute digest mismatch, wire violation, or
    /// durable authority rejection.
    pub async fn complete(
        &self,
        request: CompleteProviderOperation,
        idempotency_key: IdempotencyKey,
    ) -> Result<ProviderExecutionReceipt, OutOfProcessProviderError> {
        let payload = encode_complete_request_proto(&request)
            .map_err(|error| OutOfProcessProviderError::Wire(error.to_string()))?;
        let decoded = self
            .exchange_typed(
                COMPLETE_PROVIDER_METHOD,
                MethodSemantics::MUTATION,
                payload,
                Some(idempotency_key),
            )
            .await?;
        match decoded {
            DecodedProviderResponse::CompleteAck(receipt) => {
                let expected_digest = derive_worker_compute_digest(
                    request.handle,
                    request.callback_id,
                    &request.seed,
                );
                if receipt.compute_digest != expected_digest {
                    return Err(OutOfProcessProviderError::ComputeDigestMismatch);
                }
                Ok(receipt)
            }
            _ => Err(OutOfProcessProviderError::Wire(
                ProtoCodecError::UnexpectedResponseArm.to_string(),
            )),
        }
    }

    /// Requests idempotent cancel-epoch fenced cancellation over the
    /// out-of-process `nlos.sabi.DriverProvider` channel.
    ///
    /// # Errors
    ///
    /// Returns [`OutOfProcessProviderError`] on transport unreachability,
    /// handshake rejection, wire violation, or cancel-epoch conflict.
    pub async fn cancel(
        &self,
        request: CancelProviderOperation,
        idempotency_key: IdempotencyKey,
    ) -> Result<CancelProviderOutcome, OutOfProcessProviderError> {
        let payload = encode_cancel_request_proto(&request)
            .map_err(|error| OutOfProcessProviderError::Wire(error.to_string()))?;
        let decoded = self
            .exchange_typed(
                CANCEL_PROVIDER_METHOD,
                MethodSemantics::MUTATION,
                payload,
                Some(idempotency_key),
            )
            .await?;
        match decoded {
            DecodedProviderResponse::CancelAck(ack) => Ok(ack),
            _ => Err(OutOfProcessProviderError::Wire(
                ProtoCodecError::UnexpectedResponseArm.to_string(),
            )),
        }
    }

    /// Queries the worker's `[TASK-PROVIDER-001]` isolation attestation and
    /// evaluates its posture locally.
    ///
    /// # Errors
    ///
    /// Returns [`OutOfProcessProviderError`] on transport unreachability,
    /// handshake rejection, or wire violation.
    pub async fn query_isolation(
        &self,
    ) -> Result<(ProviderIsolationAttestation, ProviderIsolationPosture), OutOfProcessProviderError>
    {
        let payload = encode_query_isolation_request_proto()
            .map_err(|error| OutOfProcessProviderError::Wire(error.to_string()))?;
        let decoded = self
            .exchange_typed(
                QUERY_ISOLATION_PROVIDER_METHOD,
                MethodSemantics::QUERY,
                payload,
                None,
            )
            .await?;
        match decoded {
            DecodedProviderResponse::IsolationReport(attestation) => {
                let posture = attestation.evaluate_posture();
                Ok((attestation, posture))
            }
            _ => Err(OutOfProcessProviderError::Wire(
                ProtoCodecError::UnexpectedResponseArm.to_string(),
            )),
        }
    }

    /// Sends raw payload bytes inside an authenticated SABI request envelope to
    /// exercise fail-closed wire validation on the out-of-process worker.
    ///
    /// # Errors
    ///
    /// Returns [`OutOfProcessProviderError`] if transport/handshake fails or
    /// the worker rejects the malformed/oversized/wrong-schema bytes.
    pub async fn exchange_raw_payload(
        &self,
        method: &str,
        semantics: MethodSemantics,
        raw_payload: Vec<u8>,
        idempotency_key: Option<IdempotencyKey>,
    ) -> Result<Envelope, OutOfProcessProviderError> {
        self.exchange_envelope(method, semantics, raw_payload, idempotency_key)
            .await
    }

    async fn exchange_typed(
        &self,
        method: &str,
        semantics: MethodSemantics,
        payload: Vec<u8>,
        idempotency_key: Option<IdempotencyKey>,
    ) -> Result<DecodedProviderResponse, OutOfProcessProviderError> {
        let envelope = self
            .exchange_envelope(method, semantics, payload, idempotency_key)
            .await?;
        decode_response_proto(&envelope.payload)
            .map_err(|error| OutOfProcessProviderError::Wire(error.to_string()))
    }

    async fn exchange_envelope(
        &self,
        method: &str,
        semantics: MethodSemantics,
        payload: Vec<u8>,
        idempotency_key: Option<IdempotencyKey>,
    ) -> Result<Envelope, OutOfProcessProviderError> {
        let request = self.build_exchange_request(method, payload, idempotency_key);
        let signer = Arc::clone(&self.credentials.signer);
        let framed = match authenticated_connect(
            &self.worker_config.endpoint,
            self.transport_config,
            self.credentials.principal_id,
            |digest| signer(digest),
        )
        .await
        {
            Ok(framed) => framed,
            Err(HandshakeError::Transport(ipc_error)) => {
                self.record_unreachable();
                return Err(OutOfProcessProviderError::Unreachable(
                    ipc_error.to_string(),
                ));
            }
            Err(other) => return Err(OutOfProcessProviderError::Handshake(other)),
        };

        let client = LocalRpcClient::new(framed.into_inner(), self.transport_config);
        let validated = match client.exchange_validated(request).await {
            Ok(response) => response,
            Err(IpcError::Compatibility(compat)) => {
                return Err(OutOfProcessProviderError::Wire(compat.to_string()));
            }
            Err(transport_error) => {
                self.record_unreachable();
                return Err(OutOfProcessProviderError::Unreachable(
                    transport_error.to_string(),
                ));
            }
        };

        let response = validated.envelope().clone();
        let context = validate_sabi_response_context(&response, semantics)
            .map_err(|error| OutOfProcessProviderError::Wire(error.to_string()))?;
        if let Some(failure) = context.failure.as_ref() {
            let code = SabiErrorCode::try_from(failure.code).unwrap_or(SabiErrorCode::Unspecified);
            let retry =
                RetryDirective::try_from(failure.retry).unwrap_or(RetryDirective::Unspecified);
            if code == SabiErrorCode::HostLost {
                self.record_unreachable();
                return Err(OutOfProcessProviderError::Unreachable(
                    failure.safe_message.clone(),
                ));
            }
            self.record_healthy();
            return Err(OutOfProcessProviderError::RemoteRejected {
                code,
                retry,
                safe_message: failure.safe_message.clone(),
            });
        }

        self.record_healthy();
        Ok(response)
    }

    fn build_exchange_request(
        &self,
        method: &str,
        payload: Vec<u8>,
        idempotency_key: Option<IdempotencyKey>,
    ) -> ExchangeRequest {
        let seq = self.next_request_seq.fetch_add(1, Ordering::Relaxed);
        let mut request_id = [0_u8; 16];
        request_id[..8].copy_from_slice(&std::process::id().to_be_bytes().repeat(2)[..8]);
        request_id[8..].copy_from_slice(&seq.to_be_bytes());

        ExchangeRequest {
            envelope: Some(Envelope {
                schema: Some(SchemaIdentity {
                    name: SABI_ENVELOPE_SCHEMA.to_owned(),
                    major: 1,
                    minor: 1,
                    critical_extension_ids: Vec::new(),
                    non_critical_extension_ids: Vec::new(),
                }),
                request_id: request_id.to_vec(),
                service: DRIVER_PROVIDER_SERVICE.to_owned(),
                method: method.to_owned(),
                common_context: Some(envelope::CommonContext::RequestContext(
                    SabiRequestContext {
                        caller: Some(CallerIdentity {
                            principal_id: self.credentials.principal_id.as_bytes().to_vec(),
                            application_id: self.credentials.application_id.to_vec(),
                            process_id: self.credentials.process_id.to_vec(),
                            process_generation: self.credentials.process_generation.get(),
                        }),
                        activity_context: Vec::new(),
                        task_execution_binding: None,
                        correlation_id: request_id.to_vec(),
                        idempotency_key: idempotency_key
                            .map_or_else(Vec::new, |key| key.as_bytes().to_vec()),
                        deadline_monotonic_ns: 0,
                        capability_handles: vec![CapabilityHandle {
                            slot: self.credentials.capability_slot,
                            generation: self.credentials.capability_generation.get(),
                        }],
                        reservation_handle: None,
                        proposal_or_input_digest_sha256: Vec::new(),
                    },
                )),
                payload,
            }),
        }
    }

    fn record_unreachable(&self) {
        *self.lock_health() = CacheHealth::Degraded {
            cause: DegradationCause::ProviderUnreachable,
        };
        if let Some(cache) = self.linked_cache.as_ref() {
            cache
                .provider()
                .arm_fault(ProviderFaultMode::FailProviderRpc);
            let _ = cache.complete(health_sync_probe_operation());
        }
    }

    fn record_healthy(&self) {
        *self.lock_health() = CacheHealth::Healthy;
        if let Some(cache) = self.linked_cache.as_ref()
            && cache.health() != CacheHealth::Healthy
        {
            cache.provider().disarm_fault();
            // Read-only lookup of a reserved all-FF probe handle returns
            // `StoreError::Operation(InvalidGeneration)` with zero durable
            // mutation, restoring `ProviderCache::health()` to `Healthy`.
            let _ = cache.complete(health_sync_probe_operation());
        }
    }

    fn lock_worker(&self) -> MutexGuard<'_, Option<SupervisedWorker>> {
        self.worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_health(&self) -> MutexGuard<'_, CacheHealth> {
        self.health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for OutOfProcessProviderDriver {
    fn drop(&mut self) {
        let _ = self.kill_worker();
    }
}

/// Shared consumer-side cache wrapper over [`OutOfProcessProviderDriver`],
/// exposing the same observational [`CacheHealth`] contract as [`ProviderCache`].
pub struct OutOfProcessProviderCache {
    driver: Arc<OutOfProcessProviderDriver>,
}

impl OutOfProcessProviderCache {
    #[must_use]
    pub const fn new(driver: Arc<OutOfProcessProviderDriver>) -> Self {
        Self { driver }
    }

    #[must_use]
    pub fn driver(&self) -> &OutOfProcessProviderDriver {
        &self.driver
    }

    #[must_use]
    pub fn health(&self) -> CacheHealth {
        self.driver.health()
    }
}

fn health_sync_probe_operation() -> CompleteProviderOperation {
    CompleteProviderOperation {
        handle: OperationHandle {
            operation_id: OperationId::from_bytes([0xFF; 16]),
            generation: Generation::new(NonZeroU64::MAX),
        },
        callback_id: CallbackId::from_bytes([0xFF; 16]),
        seed: [0xFF; 32],
    }
}

fn spawn_worker_process(
    config: &ProviderWorkerConfig,
) -> Result<SupervisedWorker, OutOfProcessProviderError> {
    let mut command = Command::new(&config.worker_binary);
    command
        .arg("--store-root")
        .arg(&config.store_root)
        .arg("--identity-root")
        .arg(&config.identity_root)
        .arg("--clock-root")
        .arg(&config.clock_root)
        .arg("--endpoint")
        .arg(&config.endpoint)
        .arg("--isolation-profile")
        .arg(config.isolation_profile.as_cli_str())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(digest) = config.attestation_digest_override.as_ref() {
        command.arg("--attestation-hex").arg(encode_hex(digest));
    }
    if let Some(principal) = config.allowed_principal {
        command
            .arg("--allowed-principal-hex")
            .arg(encode_hex(principal.as_bytes()));
    }
    if let Some(max_requests) = config.max_requests {
        command.arg("--max-requests").arg(max_requests.to_string());
    }

    let mut child = command.spawn().map_err(|error| {
        OutOfProcessProviderError::WorkerProcess(format!(
            "failed to spawn {}: {error}",
            config.worker_binary.display()
        ))
    })?;
    let expected_pid = child.id();
    let stdout = child.stdout.take().ok_or_else(|| {
        OutOfProcessProviderError::WorkerProcess("worker stdout pipe missing".to_owned())
    })?;

    let (tx, rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let result = reader
            .read_line(&mut line)
            .map(|_| line)
            .map_err(|err| err.to_string());
        let _ = tx.send(result);
    });

    match rx.recv_timeout(WORKER_STARTUP_TIMEOUT) {
        Ok(Ok(line)) => {
            let trimmed = line.trim();
            let reported_pid = parse_ready_pid(trimmed).ok_or_else(|| {
                let _ = child.kill();
                let _ = child.wait();
                OutOfProcessProviderError::WorkerProcess(format!(
                    "worker emitted unexpected readiness line {trimmed:?}"
                ))
            })?;
            if reported_pid != expected_pid {
                let _ = child.kill();
                let _ = child.wait();
                return Err(OutOfProcessProviderError::WorkerProcess(format!(
                    "worker reported pid {reported_pid}, expected {expected_pid}"
                )));
            }
            Ok(SupervisedWorker {
                child,
                pid: expected_pid,
            })
        }
        Ok(Err(read_err)) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(OutOfProcessProviderError::WorkerProcess(format!(
                "failed reading worker readiness line: {read_err}"
            )))
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(OutOfProcessProviderError::WorkerProcess(
                "timed out waiting for worker READY line".to_owned(),
            ))
        }
    }
}

fn parse_ready_pid(line: &str) -> Option<u32> {
    let rest = line.strip_prefix(WORKER_READY_PREFIX)?;
    let (_, pid_str) = rest.rsplit_once(" pid=")?;
    let endpoint_part = rest.strip_suffix(&format!(" pid={pid_str}"))?;
    if !endpoint_part.starts_with("endpoint=") {
        return None;
    }
    pid_str.parse::<u32>().ok()
}

// ============================================================================
// Server-side `nlos.sabi.DriverProvider` SABI envelope handler
// ============================================================================

/// Server-side typed error surface for `nlos-provider-worker`.
#[derive(Debug)]
pub enum DriverProviderServiceError {
    Payload(CompatibilityError),
    Proto(ProtoCodecError),
    Common(CommonSemanticsError),
    UnknownMethod,
    AuthorizationDenied(&'static str),
    ProviderUnreachable,
    Store(StoreError),
}

impl fmt::Display for DriverProviderServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Payload(error) => write!(formatter, "invalid envelope: {error}"),
            Self::Proto(error) => write!(formatter, "invalid DriverProvider payload: {error}"),
            Self::Common(error) => write!(formatter, "invalid SABI request context: {error}"),
            Self::UnknownMethod => formatter.write_str("unknown DriverProvider service or method"),
            Self::AuthorizationDenied(reason) => {
                write!(formatter, "DriverProvider authorization denied: {reason}")
            }
            Self::ProviderUnreachable => {
                formatter.write_str("provider rpc boundary is unreachable")
            }
            Self::Store(error) => write!(formatter, "operation authority rejected call: {error}"),
        }
    }
}

impl Error for DriverProviderServiceError {}

impl From<CompatibilityError> for DriverProviderServiceError {
    fn from(error: CompatibilityError) -> Self {
        Self::Payload(error)
    }
}

impl From<ProtoCodecError> for DriverProviderServiceError {
    fn from(error: ProtoCodecError) -> Self {
        Self::Proto(error)
    }
}

impl From<CommonSemanticsError> for DriverProviderServiceError {
    fn from(error: CommonSemanticsError) -> Self {
        Self::Common(error)
    }
}

impl From<StoreError> for DriverProviderServiceError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<ProviderError> for DriverProviderServiceError {
    fn from(error: ProviderError) -> Self {
        match error {
            ProviderError::Unreachable => Self::ProviderUnreachable,
            ProviderError::Store(store) => Self::Store(store),
        }
    }
}

impl DriverProviderServiceError {
    #[must_use]
    pub fn to_sabi_failure(&self) -> SabiFailure {
        let (code, retry, safe_message) = match self {
            Self::Payload(_) | Self::Proto(_) => (
                SabiErrorCode::InvalidArgument,
                RetryDirective::DoNotRetry,
                "request violates the frozen DriverProvider payload contract",
            ),
            Self::Common(CommonSemanticsError::DeadlineExpired) => (
                SabiErrorCode::Deadline,
                RetryDirective::DoNotRetry,
                "call deadline has expired",
            ),
            Self::Common(_) => (
                SabiErrorCode::InvalidArgument,
                RetryDirective::DoNotRetry,
                "request violates the common SABI contract",
            ),
            Self::UnknownMethod => (
                SabiErrorCode::NotSupported,
                RetryDirective::DoNotRetry,
                "unknown DriverProvider service or method",
            ),
            Self::AuthorizationDenied(_) => (
                SabiErrorCode::Rights,
                RetryDirective::DoNotRetry,
                "DriverProvider authorization denied",
            ),
            Self::ProviderUnreachable => (
                SabiErrorCode::HostLost,
                RetryDirective::RetrySameIdempotencyKey,
                "provider rpc boundary is unreachable; retry the same request bytes",
            ),
            Self::Store(store) => store_failure_class(store),
        };
        SabiFailure {
            code: code.into(),
            retry: retry.into(),
            safe_message: safe_message.to_owned(),
        }
    }
}

fn store_failure_class(error: &StoreError) -> (SabiErrorCode, RetryDirective, &'static str) {
    use nlos_operation::OperationError as Operation;

    match error {
        StoreError::Sqlite(_) => (
            SabiErrorCode::Durability,
            RetryDirective::RetrySameIdempotencyKey,
            "operation authority storage failure; retry with the same request bytes",
        ),
        StoreError::DurabilityUnavailable { .. } => (
            SabiErrorCode::Durability,
            RetryDirective::DoNotRetry,
            "operation authority durability configuration is unavailable",
        ),
        StoreError::UnsupportedSchema(_) => (
            SabiErrorCode::Driver,
            RetryDirective::DoNotRetry,
            "operation authority schema version is unsupported",
        ),
        StoreError::OutboxEntryNotFound
        | StoreError::OutboxParkConflict
        | StoreError::InvalidParkRequest(_)
        | StoreError::InvalidIdempotencyScope
        | StoreError::IdempotencyRecordNotFound
        | StoreError::DurableResultTooLarge { .. } => (
            SabiErrorCode::Driver,
            RetryDirective::DoNotRetry,
            "DriverProvider request hit an operation authority path it never issues",
        ),
        StoreError::IdempotencyConflict => (
            SabiErrorCode::Conflict,
            RetryDirective::DoNotRetry,
            "idempotency key conflicts with durable operation state",
        ),
        StoreError::DispatchPreparationNotFound => (
            SabiErrorCode::NotFound,
            RetryDirective::DoNotRetry,
            "operation has no durable dispatch preparation",
        ),
        StoreError::DispatchPreparationConflict | StoreError::OperationNotActivated => (
            SabiErrorCode::Conflict,
            RetryDirective::DoNotRetry,
            "dispatch preparation conflicts with the durable owner-bound request",
        ),
        StoreError::CancelEpochConflict { .. } => (
            SabiErrorCode::Conflict,
            RetryDirective::DoNotRetry,
            "operation cancel epoch conflicts with durable state",
        ),
        StoreError::Operation(Operation::DuplicateOperation) => (
            SabiErrorCode::Conflict,
            RetryDirective::DoNotRetry,
            "operation id was reused for a different specification",
        ),
        StoreError::Operation(Operation::InvalidGeneration) => (
            SabiErrorCode::Conflict,
            RetryDirective::DoNotRetry,
            "operation generation is stale or the callback identity is unknown",
        ),
        StoreError::Operation(Operation::InvalidState) => (
            SabiErrorCode::State,
            RetryDirective::DoNotRetry,
            "operation state does not allow this transition",
        ),
        StoreError::Operation(Operation::CancelEpochExhausted) => (
            SabiErrorCode::State,
            RetryDirective::DoNotRetry,
            "operation cancellation epoch is exhausted",
        ),
        StoreError::Operation(Operation::CallbackIdentityConflict) => (
            SabiErrorCode::Conflict,
            RetryDirective::DoNotRetry,
            "callback identity was reused with different completion data",
        ),
        StoreError::CorruptRecord(_) | StoreError::LockPoisoned => (
            SabiErrorCode::Driver,
            RetryDirective::DoNotRetry,
            "local operation authority defect; do not retry",
        ),
    }
}

/// Typed `nlos.sabi.DriverProvider` SABI handler served by `nlos-provider-worker`.
pub struct DriverProviderService {
    provider: Arc<MockProvider>,
    attestation: ProviderIsolationAttestation,
    allowed_principal: Option<PrincipalId>,
}

impl DriverProviderService {
    #[must_use]
    pub const fn new(
        provider: Arc<MockProvider>,
        attestation: ProviderIsolationAttestation,
        allowed_principal: Option<PrincipalId>,
    ) -> Self {
        Self {
            provider,
            attestation,
            allowed_principal,
        }
    }

    #[must_use]
    pub fn handle_for_ipc(
        &self,
        request: &Envelope,
        verified_principal: PrincipalId,
        now_monotonic_ns: u64,
    ) -> Envelope {
        match self.handle(request, verified_principal, now_monotonic_ns) {
            Ok(response) => response,
            Err(error) => provider_failure_envelope(request, &error),
        }
    }

    /// Executes one authenticated `DriverProvider` request envelope.
    ///
    /// # Errors
    ///
    /// Fails closed before touching `SqliteOperationStore` on unknown method,
    /// invalid SABI context, principal mismatch, unauthorized principal,
    /// missing capability handle, or malformed/wrong-schema protobuf bytes.
    pub fn handle(
        &self,
        request: &Envelope,
        verified_principal: PrincipalId,
        now_monotonic_ns: u64,
    ) -> Result<Envelope, DriverProviderServiceError> {
        if request.service != DRIVER_PROVIDER_SERVICE {
            return Err(DriverProviderServiceError::UnknownMethod);
        }
        let semantics = match request.method.as_str() {
            REGISTER_PROVIDER_METHOD
            | DISPATCH_PROVIDER_METHOD
            | COMPLETE_PROVIDER_METHOD
            | CANCEL_PROVIDER_METHOD => MethodSemantics::MUTATION,
            QUERY_ISOLATION_PROVIDER_METHOD => MethodSemantics::QUERY,
            _ => return Err(DriverProviderServiceError::UnknownMethod),
        };

        let context = validate_sabi_request_context(request, semantics, now_monotonic_ns)?;
        self.authorize_caller(verified_principal, context, semantics)?;

        let decoded = decode_request_proto(&request.payload)?;
        if decoded.expected_method() != request.method {
            return Err(DriverProviderServiceError::Proto(
                ProtoCodecError::UnexpectedResponseArm,
            ));
        }

        let correlation_id = context.correlation_id.clone();
        match decoded {
            DecodedProviderRequest::Register(spec) => {
                let outcome = self.provider.register(spec)?;
                let payload = encode_register_ack_proto(&outcome)?;
                Ok(provider_response_envelope(
                    request,
                    correlation_id,
                    Some(op_ref(outcome.handle)),
                    vec![ReceiptReference {
                        receipt_id: outcome.admission_receipt_id.as_bytes().to_vec(),
                    }],
                    payload,
                ))
            }
            DecodedProviderRequest::Dispatch(dispatch) => {
                let outcome = self.provider.dispatch(dispatch)?;
                let payload = encode_dispatch_ack_proto(&outcome)?;
                Ok(provider_response_envelope(
                    request,
                    correlation_id,
                    Some(op_ref(outcome.ticket.operation)),
                    vec![
                        ReceiptReference {
                            receipt_id: outcome.preparation_receipt_id.as_bytes().to_vec(),
                        },
                        ReceiptReference {
                            receipt_id: outcome.activation_receipt_id.as_bytes().to_vec(),
                        },
                    ],
                    payload,
                ))
            }
            DecodedProviderRequest::Complete(complete) => {
                self.handle_complete(request, correlation_id, complete)
            }
            DecodedProviderRequest::Cancel(cancel) => {
                self.handle_cancel(request, correlation_id, cancel)
            }
            DecodedProviderRequest::QueryIsolation => {
                let payload = encode_isolation_report_proto(&self.attestation)?;
                Ok(provider_response_envelope(
                    request,
                    correlation_id,
                    None,
                    Vec::new(),
                    payload,
                ))
            }
        }
    }

    fn authorize_caller(
        &self,
        verified_principal: PrincipalId,
        context: &SabiRequestContext,
        semantics: MethodSemantics,
    ) -> Result<(), DriverProviderServiceError> {
        if let Some(allowed) = self.allowed_principal
            && allowed != verified_principal
        {
            return Err(DriverProviderServiceError::AuthorizationDenied(
                "verified principal is not authorized for this provider worker",
            ));
        }
        let caller_matches = context
            .caller
            .as_ref()
            .is_some_and(|caller| caller.principal_id.as_slice() == verified_principal.as_bytes());
        if !caller_matches {
            return Err(DriverProviderServiceError::AuthorizationDenied(
                "caller principal does not match verified handshake principal",
            ));
        }
        if semantics.side_effecting && context.capability_handles.is_empty() {
            return Err(DriverProviderServiceError::AuthorizationDenied(
                "side-effecting provider request requires a capability handle",
            ));
        }
        Ok(())
    }

    fn handle_complete(
        &self,
        request: &Envelope,
        correlation_id: Vec<u8>,
        complete: CompleteProviderOperation,
    ) -> Result<Envelope, DriverProviderServiceError> {
        let outcome = self.provider.complete(complete)?;
        let activation = self
            .provider
            .store()
            .inspect_activation_proof(complete.handle)?;
        let snapshot = self.provider.store().inspect(complete.handle)?;
        let fenced_reconciliation_only = activation.cancel_epoch < snapshot.cancel_epoch;
        let compute_digest =
            derive_worker_compute_digest(complete.handle, complete.callback_id, &complete.seed);
        let payload = encode_complete_ack_proto(
            &outcome,
            compute_digest,
            fenced_reconciliation_only,
            &self.attestation,
        )?;
        let receipt_id = terminal_receipt_of_state(outcome.state)
            .ok_or(ProtoCodecError::StateOutcomeMismatch)?;
        Ok(provider_response_envelope(
            request,
            correlation_id,
            Some(op_ref(complete.handle)),
            vec![ReceiptReference {
                receipt_id: receipt_id.as_bytes().to_vec(),
            }],
            payload,
        ))
    }

    fn handle_cancel(
        &self,
        request: &Envelope,
        correlation_id: Vec<u8>,
        cancel: CancelProviderOperation,
    ) -> Result<Envelope, DriverProviderServiceError> {
        let decision = self.provider.store().request_cancel_idempotent(
            cancel.handle,
            cancel.expected_cancel_epoch,
            cancel.no_effect_receipt_id,
        )?;
        let outcome = match decision {
            CancelRequestDecision::Applied(snapshot) => {
                CancelProviderOutcome::from_snapshot(false, false, snapshot)
            }
            CancelRequestDecision::Replayed(snapshot) => {
                CancelProviderOutcome::from_snapshot(true, false, snapshot)
            }
            CancelRequestDecision::AlreadyTerminal(snapshot) => {
                CancelProviderOutcome::from_snapshot(false, true, snapshot)
            }
        };
        let receipts = terminal_receipt_of_state(outcome.state)
            .into_iter()
            .map(|id| ReceiptReference {
                receipt_id: id.as_bytes().to_vec(),
            })
            .collect();
        let payload = encode_cancel_ack_proto(&outcome)?;
        Ok(provider_response_envelope(
            request,
            correlation_id,
            Some(op_ref(outcome.handle)),
            receipts,
            payload,
        ))
    }
}

fn op_ref(handle: OperationHandle) -> OperationReference {
    OperationReference {
        operation_id: handle.operation_id.as_bytes().to_vec(),
        generation: handle.generation.get(),
    }
}

fn provider_response_envelope(
    request: &Envelope,
    correlation_id: Vec<u8>,
    operation: Option<OperationReference>,
    receipts: Vec<ReceiptReference>,
    payload: Vec<u8>,
) -> Envelope {
    let mut response = request.clone();
    response.payload = payload;
    response.common_context = Some(envelope::CommonContext::ResponseContext(
        SabiResponseContext {
            correlation_id,
            operation,
            receipts,
            failure: None,
        },
    ));
    response
}

fn provider_failure_envelope(request: &Envelope, error: &DriverProviderServiceError) -> Envelope {
    let correlation_id = match request.common_context.as_ref() {
        Some(envelope::CommonContext::RequestContext(context))
            if context.correlation_id.len() == REQUEST_ID_BYTES =>
        {
            context.correlation_id.clone()
        }
        _ if request.request_id.len() == REQUEST_ID_BYTES => request.request_id.clone(),
        _ => vec![0; REQUEST_ID_BYTES],
    };
    let failure = error.to_sabi_failure();
    let mut response = request.clone();
    response.payload.clear();
    response.common_context = Some(envelope::CommonContext::ResponseContext(
        SabiResponseContext {
            correlation_id,
            operation: None,
            receipts: Vec::new(),
            failure: Some(failure),
        },
    ));
    response
}

/// Encodes fixed bytes as lowercase hexadecimal.
#[must_use]
pub fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Decodes a fixed-width hexadecimal string into `[u8; N]`.
///
/// # Errors
///
/// Fails closed if `input` does not contain `2 * N` ASCII hex digits.
pub fn decode_hex_exact<const N: usize>(input: &str) -> Result<[u8; N], String> {
    let trimmed = input.trim();
    if trimmed.len() != N * 2 || !trimmed.is_ascii() {
        return Err(format!(
            "expected {} hex characters, got {}",
            N * 2,
            trimmed.len()
        ));
    }
    let mut out = [0_u8; N];
    for (index, slot) in out.iter_mut().enumerate() {
        let offset = index * 2;
        *slot = u8::from_str_radix(&trimmed[offset..offset + 2], 16)
            .map_err(|err| format!("invalid hex byte at offset {offset}: {err}"))?;
    }
    Ok(out)
}
