//! Trusted library boundary for the two `ToolSandbox` wire methods.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nlos_capability::service_call::{
    AuthorizationError, CallBinding, ServiceAction, ServiceAuthorizer, ServiceMethod,
    ServiceObject, ServiceTarget, ServiceTime, authorize_call,
};
use nlos_schema::sabi::v1::{
    Envelope, InvokeToolRequest, InvokeToolResult, ListToolsRequest, ListToolsResult,
    ToolDescriptorWire, envelope::CommonContext,
};
use nlos_schema::{
    CompatibilityError, MethodSemantics, SABI_ENVELOPE_SCHEMA, decode_invoke_tool_request,
    decode_list_tools_request, encode_invoke_tool_request, encode_invoke_tool_result,
    encode_list_tools_request, encode_list_tools_result, encode_sabi_envelope,
    tool_sandbox_schema_identity, validate_sabi_request_context,
};
use nlos_types::{Generation, NamespaceId, OperationId};
use sha2::{Digest, Sha256};

use crate::error::HostOutcome;
use crate::host_bin::EXIT_TOOL_WATCHDOG;
use crate::process::HostProcessRequest;
use crate::tool_host_protocol::{
    MAX_FILE_WRITE, MAX_GUEST_STDERR, MAX_STDOUT, MAX_WALL_MS, ToolHostLimits, artifact_digest,
};
use crate::tool_json::{self, Json};
use crate::tool_operation_store::{
    BeginDecision, DirectoryEvidence, DurableToolResult, StoreError, ToolFailure, ToolHostQuota,
    ToolOperation, ToolOperationHandle, ToolOperationKey, ToolOperationSpec, ToolOperationState,
    ToolOperationStore, ToolOwner,
};
use crate::tool_process::{self, ChildControl, HostStop};
use crate::{PreopenGrant, SandboxProfile, tool_reconcile};

pub const SERVICE_NAME: &str = "tool_sandbox";
pub const LIST_TOOLS: &str = "list_tools";
pub const INVOKE_TOOL: &str = "invoke_tool";
pub const CATALOG_ENTRYPOINT: &str = "__nlos_tool_catalog__";
const MAX_TOOLS: usize = 128;
const MAX_PREOPENS: usize = 16;
const MAX_NAME: usize = 128;
const MAX_SUMMARY: usize = 1024;
const MAX_INPUT_SCHEMA: usize = 16 * 1024;
const INPUT_MOUNT: &str = "/__nlos_input__";

pub type ToolServiceClock = dyn Fn() -> Result<ServiceTime, AuthorizationError> + Send + Sync;

/// Host-supplied registry entry. The artifact and preopen roots belong in
/// host-managed locations that untrusted callers cannot replace or remount.
pub struct ToolRegistration {
    pub id: [u8; 16],
    pub namespace: NamespaceId,
    pub generation: Generation,
    pub artifact: PathBuf,
    pub tools: Vec<RegisteredTool>,
}

pub struct RegisteredTool {
    pub name: String,
    pub entrypoint: String,
    pub profile: SandboxProfile,
}

/// Sidecar for a trusted materializer. The embedding must independently
/// verify access and content, and keep input.json stable for the run.
pub struct PreparedToolInput {
    pub binding: CallBinding,
    pub registration: [u8; 16],
    pub request_digest: [u8; 32],
    pub content_digest: [u8; 32],
    pub preopen: PreopenGrant,
}

#[derive(Clone, Copy, Debug)]
pub struct ToolServiceLimits {
    pub inline_bytes: usize,
    pub json_depth: usize,
    pub stdout_bytes: usize,
    pub guest_stderr_bytes: usize,
    pub write_bytes: u64,
    pub wall_ms: u64,
    pub active_hosts: usize,
    pub active_hosts_per_principal: usize,
    pub reconciliation_entries: usize,
    pub reconciliation_bytes: u64,
}

impl Default for ToolServiceLimits {
    fn default() -> Self {
        Self {
            inline_bytes: 8192,
            json_depth: 64,
            stdout_bytes: MAX_STDOUT,
            guest_stderr_bytes: MAX_GUEST_STDERR,
            write_bytes: MAX_FILE_WRITE,
            wall_ms: MAX_WALL_MS,
            active_hosts: 16,
            active_hosts_per_principal: 4,
            reconciliation_entries: tool_reconcile::MAX_ENTRIES,
            reconciliation_bytes: tool_reconcile::MAX_SCAN_BYTES,
        }
    }
}

impl ToolServiceLimits {
    fn valid(self) -> bool {
        let max = Self::default();
        (1..=max.inline_bytes).contains(&self.inline_bytes)
            && (1..=max.json_depth).contains(&self.json_depth)
            && self.host_limits().valid()
            && (1..=max.wall_ms).contains(&self.wall_ms)
            && (1..=max.active_hosts).contains(&self.active_hosts)
            && (1..=max.active_hosts_per_principal).contains(&self.active_hosts_per_principal)
            && self.reconciliation_entries <= max.reconciliation_entries
            && self.reconciliation_bytes <= max.reconciliation_bytes
    }

    fn host_limits(self) -> ToolHostLimits {
        ToolHostLimits {
            stdout: self.stdout_bytes,
            stderr: self.guest_stderr_bytes,
            file_write: self.write_bytes,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolServiceError {
    Authorization(AuthorizationError),
    Store(StoreError),
    InvalidEnvelope,
    InvalidPayload,
    FrameTooLarge,
    BindingMismatch,
    UnsupportedTaskBinding,
    DigestMismatch,
    InvalidRegistry,
    UnknownTool,
    InvalidCatalog,
    InlineLimit,
    InvalidJson,
    PreparedInputDenied,
    NoHostPermit,
    ConflictingHostLimits,
    ArtifactUnavailable,
    Host(ToolFailure),
}

impl fmt::Display for ToolServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tool sandbox: {self:?}")
    }
}
impl std::error::Error for ToolServiceError {}
impl From<AuthorizationError> for ToolServiceError {
    fn from(error: AuthorizationError) -> Self {
        Self::Authorization(error)
    }
}
impl From<StoreError> for ToolServiceError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<ToolFailure> for ToolServiceError {
    fn from(error: ToolFailure) -> Self {
        Self::Host(error)
    }
}

struct Registration {
    namespace: NamespaceId,
    generation: Generation,
    artifact: PathBuf,
    tools: BTreeMap<String, RegisteredTool>,
    catalog: Mutex<Option<([u8; 32], Arc<Catalog>)>>,
}

struct Catalog {
    wire: Vec<u8>,
    read_only: BTreeMap<String, bool>,
}

type Running = Mutex<HashMap<OperationId, Arc<ChildControl>>>;

struct ActiveOperation<'a> {
    running: &'a Running,
    id: OperationId,
    child: Arc<ChildControl>,
}

impl Drop for ActiveOperation<'_> {
    fn drop(&mut self) {
        let mut running = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        running.remove(&self.id);
    }
}

pub struct ToolSandboxService {
    host: PathBuf,
    registrations: BTreeMap<[u8; 16], Registration>,
    clock: Arc<ToolServiceClock>,
    policy: Arc<dyn ServiceAuthorizer>,
    store: Box<dyn ToolOperationStore>,
    limits: ToolServiceLimits,
    host_quota: Arc<ToolHostQuota>,
    running: Running,
}

impl ToolSandboxService {
    /// # Errors
    /// Rejects invalid or ambiguous trusted registry configuration and host
    /// limits that conflict with those already bound to this store authority.
    pub fn new(
        host: impl AsRef<Path>,
        registrations: Vec<ToolRegistration>,
        clock: Arc<ToolServiceClock>,
        policy: Arc<dyn ServiceAuthorizer>,
        store: Box<dyn ToolOperationStore>,
        limits: ToolServiceLimits,
    ) -> Result<Self, ToolServiceError> {
        let host = host.as_ref();
        if !host.is_absolute() || !limits.valid() || registrations.len() > MAX_TOOLS {
            return Err(ToolServiceError::InvalidRegistry);
        }
        let host = host
            .canonicalize()
            .map_err(|_| ToolServiceError::InvalidRegistry)?;
        if !host.is_file() {
            return Err(ToolServiceError::InvalidRegistry);
        }
        let mut registry = BTreeMap::new();
        for registration in registrations {
            if registry
                .insert(registration.id, Registration::new(registration)?)
                .is_some()
            {
                return Err(ToolServiceError::InvalidRegistry);
            }
        }
        let host_quota = store.host_quota();
        if !host_quota.bind(limits.active_hosts, limits.active_hosts_per_principal) {
            return Err(ToolServiceError::ConflictingHostLimits);
        }
        Ok(Self {
            host,
            registrations: registry,
            clock,
            policy,
            store,
            limits,
            host_quota,
            running: Mutex::default(),
        })
    }

    /// Returns the complete, bounded catalog for a trusted registry route.
    ///
    /// # Errors
    /// Returns a codec, caller binding, authorization, catalog or host error.
    pub fn list_tools(
        &self,
        registration: [u8; 16],
        call: &CallBinding,
        envelope: &Envelope,
    ) -> Result<Vec<u8>, ToolServiceError> {
        decode_list_tools_request(&envelope.payload).map_err(|error| codec_error(&error))?;
        let now = (self.clock)()?;
        check_envelope(
            call,
            envelope,
            LIST_TOOLS,
            MethodSemantics::LONG_RUNNING_QUERY,
            now,
        )?;
        let digest = list_input_digest(registration);
        check_digest(envelope, digest)?;
        let registered = self.registration(registration)?;
        let action = registered.action(ServiceMethod::ListTools, digest)?;
        self.authorize(call, &action, now)?;
        let catalog = self.catalog(call, registered, &action)?;
        self.authorize(call, &action, (self.clock)()?)?;
        Ok(catalog.wire.clone())
    }

    /// Runs inline JSON and returns the durable state. A concurrent retry
    /// can obtain the handle while this call is running.
    ///
    /// # Errors
    /// Returns a codec, binding, authorization, admission or storage error.
    pub fn invoke_tool(
        &self,
        registration: [u8; 16],
        call: &CallBinding,
        envelope: &Envelope,
    ) -> Result<ToolOperation, ToolServiceError> {
        self.invoke(registration, call, envelope, None)
    }

    /// Invokes using an input prepared by the trusted embedding.
    ///
    /// # Errors
    /// Rejects mismatched or writable prepared inputs and normal invoke failures.
    pub fn invoke_tool_with_prepared_input(
        &self,
        registration: [u8; 16],
        call: &CallBinding,
        envelope: &Envelope,
        input: &PreparedToolInput,
    ) -> Result<ToolOperation, ToolServiceError> {
        self.invoke(registration, call, envelope, Some(input))
    }

    /// Replays an inline invocation without creating an operation or spawning.
    ///
    /// # Errors
    /// Returns a codec, binding, authorization or idempotency/storage error.
    pub fn replay(
        &self,
        registration: [u8; 16],
        call: &CallBinding,
        envelope: &Envelope,
    ) -> Result<Option<ToolOperation>, ToolServiceError> {
        let request = self.prepare_invoke(registration, call, envelope, None)?;
        let existing = self
            .store
            .find(request.key, request.action.input_digest, request.spec)?;
        self.authorize(call, &request.action, (self.clock)()?)?;
        Ok(existing)
    }

    /// # Errors
    /// Returns an authorization or storage error, including a stale handle.
    pub fn query_operation(
        &self,
        call: &CallBinding,
        handle: ToolOperationHandle,
    ) -> Result<ToolOperation, ToolServiceError> {
        self.operation_for(call, handle, ServiceMethod::QueryToolOperation)
    }

    /// Requests cancellation durably. The local host is killed and reaped
    /// inline, and a host owned by another executor observes the same durable
    /// request during its bounded polling loop. A `CancelRequested` snapshot
    /// acknowledges the request while the executor settles the terminal state.
    ///
    /// # Errors
    /// Returns an authorization, storage or host-containment error.
    pub fn cancel_operation(
        &self,
        call: &CallBinding,
        handle: ToolOperationHandle,
    ) -> Result<ToolOperation, ToolServiceError> {
        loop {
            let operation = self.operation_for(call, handle, ServiceMethod::CancelToolOperation)?;
            match self.store.request_cancel(&operation) {
                Ok(requested) => {
                    if requested.state == ToolOperationState::CancelRequested {
                        let child = self
                            .running
                            .lock()
                            .map_err(|_| StoreError::Durability)?
                            .get(&handle.id)
                            .cloned();
                        if let Some(child) = child {
                            child
                                .cancel()
                                .map_err(|_| ToolServiceError::Host(ToolFailure::HostLost))?;
                        }
                    }
                    return Ok(requested);
                }
                Err(StoreError::Stale) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn operation_for(
        &self,
        call: &CallBinding,
        handle: ToolOperationHandle,
        method: ServiceMethod,
    ) -> Result<ToolOperation, ToolServiceError> {
        let operation = self.store.query(handle).map_err(|error| match error {
            StoreError::NotFound | StoreError::Stale => {
                ToolServiceError::Authorization(AuthorizationError::Denied)
            }
            error => error.into(),
        })?;
        if operation.spec.origin != call.caller {
            return Err(AuthorizationError::Denied.into());
        }
        let action = ServiceAction {
            method,
            target: ServiceTarget {
                namespace: operation.key.owner.namespace,
                object: ServiceObject::ToolOperation(handle.id),
                generation: handle.generation,
            },
            input_digest: control_input_digest(method, handle),
        };
        self.authorize(call, &action, (self.clock)()?)?;
        Ok(operation)
    }

    fn authorize(
        &self,
        call: &CallBinding,
        action: &ServiceAction,
        now: ServiceTime,
    ) -> Result<(), ToolServiceError> {
        authorize_call(self.policy.as_ref(), call, action, now)?;
        Ok(())
    }

    fn registration(&self, id: [u8; 16]) -> Result<&Registration, ToolServiceError> {
        self.registrations
            .get(&id)
            .ok_or(ToolServiceError::UnknownTool)
    }

    fn catalog(
        &self,
        call: &CallBinding,
        registered: &Registration,
        action: &ServiceAction,
    ) -> Result<Arc<Catalog>, ToolServiceError> {
        let digest = wasm_target(action)?;
        if let Some((hash, catalog)) = &*registered
            .catalog
            .lock()
            .map_err(|_| ToolServiceError::InvalidCatalog)?
            && *hash == digest
        {
            return Ok(Arc::clone(catalog));
        }
        let _permit = self
            .host_quota
            .try_acquire(call.caller.principal)
            .ok_or(ToolServiceError::NoHostPermit)?;
        let (request, deadline) = self.host_request(
            call,
            registered,
            action,
            CATALOG_ENTRYPOINT,
            SandboxProfile::deny_all(),
        )?;
        let stdout = self.run(digest, &request, deadline, &ChildControl::default(), || {
            Ok(())
        })?;
        let catalog = Arc::new(Catalog::parse(&stdout, self.limits.json_depth, registered)?);
        *registered
            .catalog
            .lock()
            .map_err(|_| ToolServiceError::InvalidCatalog)? = Some((digest, Arc::clone(&catalog)));
        Ok(catalog)
    }

    fn prepare_invoke<'a>(
        &'a self,
        registration: [u8; 16],
        call: &CallBinding,
        envelope: &Envelope,
        input: Option<&PreparedToolInput>,
    ) -> Result<Invocation<'a>, ToolServiceError> {
        let request =
            decode_invoke_tool_request(&envelope.payload).map_err(|error| codec_error(&error))?;
        let now = (self.clock)()?;
        check_envelope(
            call,
            envelope,
            INVOKE_TOOL,
            MethodSemantics::LONG_RUNNING_MUTATION,
            now,
        )?;
        if request.arguments_json.len() > self.limits.inline_bytes {
            return Err(ToolServiceError::InlineLimit);
        }
        tool_json::parse(request.arguments_json.as_bytes(), self.limits.json_depth)
            .map_err(|_| ToolServiceError::InvalidJson)?;
        let digest = invoke_input_digest(registration, &request)?;
        check_digest(envelope, digest)?;
        let registered = self.registration(registration)?;
        let tool = registered
            .tools
            .get(&request.tool)
            .ok_or(ToolServiceError::UnknownTool)?;
        let mut profile = tool.profile.clone();
        let (digest, arguments) = if let Some(input) = input {
            if input.registration != registration
                || input.binding != *call
                || input.request_digest != digest
                || input.preopen.allow_write()
                || input.preopen.guest_dir() != INPUT_MOUNT
            {
                return Err(ToolServiceError::PreparedInputDenied);
            }
            profile = profile.with_preopen(input.preopen.clone());
            let digest = digest_parts(b"prepared", registration, &[&digest, &input.content_digest]);
            (
                digest,
                "{\"file\":\"/__nlos_input__/input.json\"}".to_owned(),
            )
        } else {
            (digest, request.arguments_json)
        };
        let action = registered.action(ServiceMethod::InvokeTool, digest)?;
        self.authorize(call, &action, now)?;
        let wasm_digest = wasm_target(&action)?;
        let key = ToolOperationKey {
            owner: ToolOwner {
                principal: call.caller.principal,
                application: call.caller.application,
                namespace: registered.namespace,
            },
            registration,
            key: call
                .idempotency_key
                .ok_or(AuthorizationError::MissingIdempotencyKey)?,
        };
        let spec = ToolOperationSpec {
            origin: call.caller,
            wasm_digest,
            registry_generation: registered.generation,
        };
        Ok(Invocation {
            action,
            key,
            spec,
            registered,
            tool,
            profile: profile.with_guest_args([arguments]),
        })
    }

    fn invoke(
        &self,
        registration: [u8; 16],
        call: &CallBinding,
        envelope: &Envelope,
        input: Option<&PreparedToolInput>,
    ) -> Result<ToolOperation, ToolServiceError> {
        let invocation = self.prepare_invoke(registration, call, envelope, input)?;
        if let Some(existing) = self.store.find(
            invocation.key,
            invocation.action.input_digest,
            invocation.spec,
        )? {
            self.authorize(call, &invocation.action, (self.clock)()?)?;
            return Ok(existing);
        }

        let catalog = self.catalog(call, invocation.registered, &invocation.action)?;
        let read_only = *catalog
            .read_only
            .get(&invocation.tool.name)
            .ok_or(ToolServiceError::UnknownTool)?;
        if read_only
            && invocation
                .profile
                .preopens()
                .iter()
                .any(PreopenGrant::allow_write)
        {
            return Err(ToolServiceError::InvalidRegistry);
        }
        let _permit = self
            .host_quota
            .try_acquire(call.caller.principal)
            .ok_or(ToolServiceError::NoHostPermit)?;
        self.authorize(call, &invocation.action, (self.clock)()?)?;
        let operation = match self.store.begin(
            invocation.key,
            invocation.action.input_digest,
            invocation.spec,
        )? {
            BeginDecision::Registered(operation) => operation,
            BeginDecision::Existing(operation) => {
                self.authorize(call, &invocation.action, (self.clock)()?)?;
                return Ok(operation);
            }
        };
        let child = Arc::new(ChildControl::default());
        self.running
            .lock()
            .map_err(|_| StoreError::Durability)?
            .insert(operation.handle.id, Arc::clone(&child));
        let active = ActiveOperation {
            id: operation.handle.id,
            running: &self.running,
            child,
        };
        let result = self.execute(call, &invocation, operation, &active.child)?;
        if matches!(result.state, ToolOperationState::Completed(_)) {
            self.authorize(call, &invocation.action, (self.clock)()?)?;
        }
        Ok(result)
    }

    fn execute(
        &self,
        call: &CallBinding,
        invocation: &Invocation<'_>,
        registered: ToolOperation,
        child: &ChildControl,
    ) -> Result<ToolOperation, ToolServiceError> {
        let preopens = invocation.profile.preopens();
        let before = self.observe(preopens);
        if let Err(error) = (self.clock)()
            .map_err(ToolServiceError::from)
            .and_then(|now| self.authorize(call, &invocation.action, now))
        {
            return self.finish(
                registered,
                ToolOperationState::Failed(before_dispatch_failure(error)),
                preopens,
            );
        }
        let dispatched = match self.store.mark_dispatched(&registered, before) {
            Ok(operation) => operation,
            Err(StoreError::Stale) => return Ok(self.store.query(registered.handle)?),
            Err(error) => return Err(error.into()),
        };
        let run = || {
            // Check the durable fence before the final call to the current
            // clock and policy. Store latency must not age that final grant.
            self.observe_dispatch(&dispatched)
                .map_err(host_stop_error)?;
            let (request, deadline) = self.host_request(
                call,
                invocation.registered,
                &invocation.action,
                &invocation.tool.entrypoint,
                invocation.profile.clone(),
            )?;
            let stdout = self.run(
                invocation.spec.wasm_digest,
                &request,
                deadline,
                child,
                || self.observe_dispatch(&dispatched),
            )?;
            Ok::<_, ToolServiceError>(self.encode_output(stdout)?)
        };
        let next = match run() {
            Ok(wire) => ToolOperationState::Completed(DurableToolResult::from_wire(wire)?),
            Err(ToolServiceError::Host(reason)) => self.unknown(&dispatched, reason, preopens),
            Err(error @ ToolServiceError::Authorization(_)) => {
                ToolOperationState::Failed(before_dispatch_failure(error))
            }
            Err(error) => {
                // A failed control lookup also stops the host. Try to
                // settle the dispatched fence if the store can still write.
                let unknown = self.unknown(&dispatched, ToolFailure::HostLost, preopens);
                let _ = self.finish(dispatched, unknown, preopens);
                return Err(error);
            }
        };
        self.finish(dispatched, next, preopens)
    }

    fn observe_dispatch(&self, dispatched: &ToolOperation) -> Result<(), HostStop> {
        let current = self
            .store
            .query(dispatched.handle)
            .map_err(HostStop::Control)?;
        match current.state {
            ToolOperationState::CancelRequested => Err(HostStop::Cancelled),
            ToolOperationState::Dispatched if current.receipt == dispatched.receipt => Ok(()),
            _ => Err(HostStop::Control(StoreError::Stale)),
        }
    }

    fn finish(
        &self,
        mut operation: ToolOperation,
        mut state: ToolOperationState,
        preopens: &[PreopenGrant],
    ) -> Result<ToolOperation, ToolServiceError> {
        loop {
            if operation.state.is_terminal() {
                return Ok(operation);
            }
            if operation.state == ToolOperationState::CancelRequested {
                state = self.unknown(&operation, ToolFailure::Cancelled, preopens);
            }
            match self.store.complete(&operation, state.clone()) {
                Ok(result) => return Ok(result),
                Err(StoreError::Stale) => operation = self.store.query(operation.handle)?,
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn unknown(
        &self,
        operation: &ToolOperation,
        reason: ToolFailure,
        preopens: &[PreopenGrant],
    ) -> ToolOperationState {
        ToolOperationState::EffectUnknown {
            reason,
            directory: DirectoryEvidence {
                before: operation.directory_before,
                after: self.observe(preopens),
            },
        }
    }

    fn observe(&self, preopens: &[PreopenGrant]) -> Option<[u8; 32]> {
        tool_reconcile::observe(
            preopens,
            self.limits.reconciliation_entries,
            self.limits.reconciliation_bytes,
        )
    }

    fn host_request(
        &self,
        call: &CallBinding,
        registered: &Registration,
        action: &ServiceAction,
        entrypoint: &str,
        profile: SandboxProfile,
    ) -> Result<(HostProcessRequest, Instant), ToolServiceError> {
        let observed_at = Instant::now();
        let now = (self.clock)()?;
        self.authorize(call, action, now)?;
        let remaining_ns = call
            .deadline_monotonic_ns
            .ok_or(AuthorizationError::MissingDeadline)?
            .checked_sub(now.monotonic_ns)
            .ok_or(AuthorizationError::DeadlineExpired)?;
        let profile_ms = if profile.epoch_deadline_ticks() == 0 {
            self.limits.wall_ms
        } else {
            profile.epoch_deadline_ticks() * profile.epoch_tick_interval_ms()
        };
        let budget = Duration::from_nanos(remaining_ns)
            .min(Duration::from_millis(self.limits.wall_ms.min(profile_ms)));
        let epoch_ms = u64::try_from(budget.as_millis()).unwrap_or(0);
        if epoch_ms == 0 {
            return Err(AuthorizationError::DeadlineExpired.into());
        }
        let tick_ms = epoch_ms.min(10);
        let profile = profile.with_epoch_deadline(epoch_ms / tick_ms, tick_ms);
        let request =
            HostProcessRequest::new(self.host.clone(), registered.artifact.clone(), profile)
                .with_entrypoint(entrypoint);
        Ok((request, observed_at + budget))
    }

    fn run(
        &self,
        digest: [u8; 32],
        request: &HostProcessRequest,
        deadline: Instant,
        child: &ChildControl,
        observe_control: impl FnMut() -> Result<(), HostStop>,
    ) -> Result<Vec<u8>, ToolServiceError> {
        let wall_ms = u64::try_from(
            deadline
                .saturating_duration_since(Instant::now())
                .as_millis(),
        )
        .unwrap_or(0);
        if wall_ms == 0 {
            return Err(ToolFailure::Deadline.into());
        }
        let limits = self.limits.host_limits();
        let mut command = tool_process::command(request, digest, limits, wall_ms)
            .map_err(|_| ToolFailure::HostLost)?;
        let outcome = tool_process::run(child, &mut command, deadline, limits, observe_control)
            .map_err(|_| ToolFailure::HostLost)?
            .map_err(host_stop_error)?;
        if outcome.code == Some(EXIT_TOOL_WATCHDOG) {
            return Err(ToolFailure::Deadline.into());
        }
        let report = outcome.report.ok_or(ToolFailure::HostLost)?;
        match (report.outcome, report.detail.as_str()) {
            (HostOutcome::Trap, "stdout-limit") => Err(ToolFailure::StdoutLimit.into()),
            (HostOutcome::Trap, "stderr-limit") => Err(ToolFailure::StderrLimit.into()),
            (HostOutcome::Trap, "file-write-limit") => Err(ToolFailure::FileWriteLimit.into()),
            (HostOutcome::Trap, "epoch-interrupt") => Err(ToolFailure::Deadline.into()),
            (HostOutcome::HostError, "artifact-changed") => {
                Err(ToolFailure::ArtifactChanged.into())
            }
            (HostOutcome::Completed | HostOutcome::GuestExit, _)
                if report.code == 0
                    && outcome.code == Some(0)
                    && report.guest_stdout_len == outcome.stdout.len() as u64 =>
            {
                Ok(outcome.stdout)
            }
            _ => Err(ToolFailure::GuestFailed.into()),
        }
    }

    fn encode_output(&self, stdout: Vec<u8>) -> Result<Vec<u8>, ToolFailure> {
        tool_json::parse(&stdout, self.limits.json_depth).map_err(|_| ToolFailure::InvalidJson)?;
        let output_json = String::from_utf8(stdout).map_err(|_| ToolFailure::InvalidJson)?;
        encode_invoke_tool_result(&InvokeToolResult {
            schema: Some(tool_sandbox_schema_identity()),
            output_json,
        })
        .map_err(|_| ToolFailure::FrameTooLarge)
    }
}

struct Invocation<'a> {
    action: ServiceAction,
    key: ToolOperationKey,
    spec: ToolOperationSpec,
    registered: &'a Registration,
    tool: &'a RegisteredTool,
    profile: SandboxProfile,
}

impl Registration {
    fn new(registration: ToolRegistration) -> Result<Self, ToolServiceError> {
        if !registration.artifact.is_absolute() || registration.tools.len() > MAX_TOOLS {
            return Err(ToolServiceError::InvalidRegistry);
        }
        let artifact = registration
            .artifact
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
            .and_then(|parent| {
                registration
                    .artifact
                    .file_name()
                    .map(|name| parent.join(name))
            })
            .ok_or(ToolServiceError::InvalidRegistry)?;
        let mut tools = BTreeMap::new();
        for tool in registration.tools {
            let profile = &tool.profile;
            let mut mounts = BTreeSet::new();
            if !valid_name(&tool.name)
                || !valid_name(&tool.entrypoint)
                || tool.entrypoint == CATALOG_ENTRYPOINT
                || profile.validate().is_err()
                || !profile.guest_args().is_empty()
                || profile.preopens().len() > MAX_PREOPENS
                || profile.preopens().iter().any(|grant| {
                    grant.guest_dir() == INPUT_MOUNT || !mounts.insert(grant.guest_dir().to_owned())
                })
                || profile
                    .epoch_deadline_ticks()
                    .checked_mul(profile.epoch_tick_interval_ms())
                    .is_none_or(|ms| ms > MAX_WALL_MS)
            {
                return Err(ToolServiceError::InvalidRegistry);
            }
            if tools.insert(tool.name.clone(), tool).is_some() {
                return Err(ToolServiceError::InvalidRegistry);
            }
        }
        Ok(Self {
            namespace: registration.namespace,
            generation: registration.generation,
            artifact,
            tools,
            catalog: Mutex::default(),
        })
    }

    fn action(
        &self,
        method: ServiceMethod,
        input_digest: [u8; 32],
    ) -> Result<ServiceAction, ToolServiceError> {
        let hash =
            artifact_digest(&self.artifact).map_err(|_| ToolServiceError::ArtifactUnavailable)?;
        Ok(ServiceAction {
            method,
            target: ServiceTarget {
                namespace: self.namespace,
                object: ServiceObject::ToolWasm(hash),
                generation: self.generation,
            },
            input_digest,
        })
    }
}

impl Catalog {
    fn parse(
        bytes: &[u8],
        depth: usize,
        registered: &Registration,
    ) -> Result<Self, ToolServiceError> {
        let Json::Array(entries) =
            tool_json::parse(bytes, depth).map_err(|_| ToolServiceError::InvalidCatalog)?
        else {
            return Err(ToolServiceError::InvalidCatalog);
        };
        if entries.len() > MAX_TOOLS {
            return Err(ToolServiceError::InvalidCatalog);
        }
        let mut tools = Vec::with_capacity(entries.len());
        let mut read_only = BTreeMap::new();
        for entry in entries {
            let (tool, is_read_only) = descriptor(entry, depth)?;
            if !registered.tools.contains_key(&tool.name)
                || read_only.insert(tool.name.clone(), is_read_only).is_some()
            {
                return Err(ToolServiceError::InvalidCatalog);
            }
            tools.push(tool);
        }
        let wire = encode_list_tools_result(&ListToolsResult {
            schema: Some(tool_sandbox_schema_identity()),
            tools,
        })
        .map_err(|error| codec_error(&error))?;
        Ok(Self { wire, read_only })
    }
}

fn descriptor(entry: Json, depth: usize) -> Result<(ToolDescriptorWire, bool), ToolServiceError> {
    let Json::Object(mut entry) = entry else {
        return Err(ToolServiceError::InvalidCatalog);
    };
    let name = text_field(&mut entry, "name", MAX_NAME, true)?;
    let summary = text_field(&mut entry, "summary", MAX_SUMMARY, true)?;
    let effect_class = text_field(&mut entry, "effect_class", 32, true)?;
    let compensation_handler = text_field(
        &mut entry,
        "compensation_handler",
        MAX_NAME,
        effect_class == "COMPENSATABLE",
    )?;
    let input_schema_json = text_field(&mut entry, "input_schema_json", MAX_INPUT_SCHEMA, true)?;
    let reconcile = text_field(&mut entry, "reconcile", 32, false)?;
    let read_only = match effect_class.as_str() {
        "READ_ONLY" => true,
        "REVERSIBLE" | "COMPENSATABLE" | "IRREVERSIBLE" if reconcile == "same-idempotency-key" => {
            false
        }
        _ => return Err(ToolServiceError::InvalidCatalog),
    };
    if !valid_name(&name)
        || (!compensation_handler.is_empty() && !valid_name(&compensation_handler))
        || !matches!(
            tool_json::parse(input_schema_json.as_bytes(), depth),
            Ok(Json::Object(_) | Json::Boolean)
        )
    {
        return Err(ToolServiceError::InvalidCatalog);
    }
    Ok((
        ToolDescriptorWire {
            name,
            summary,
            effect_class,
            compensation_handler,
            input_schema_json,
        },
        read_only,
    ))
}

fn text_field(
    entry: &mut BTreeMap<String, Json>,
    field: &str,
    limit: usize,
    required: bool,
) -> Result<String, ToolServiceError> {
    match entry.remove(field) {
        Some(Json::Text(text))
            if text.len() <= limit
                && (!required || !text.trim().is_empty())
                && !text.contains('\0') =>
        {
            Ok(text)
        }
        None if !required => Ok(String::new()),
        _ => Err(ToolServiceError::InvalidCatalog),
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn check_envelope(
    call: &CallBinding,
    envelope: &Envelope,
    method: &str,
    semantics: MethodSemantics,
    now: ServiceTime,
) -> Result<(), ToolServiceError> {
    if envelope.service != SERVICE_NAME
        || envelope.method != method
        || envelope
            .schema
            .as_ref()
            .is_none_or(|schema| schema.name != SABI_ENVELOPE_SCHEMA)
    {
        return Err(ToolServiceError::InvalidEnvelope);
    }
    let context =
        validate_sabi_request_context(envelope, semantics, now.monotonic_ns).map_err(|error| {
            match error {
                nlos_schema::CommonSemanticsError::DeadlineExpired => {
                    ToolServiceError::Authorization(AuthorizationError::DeadlineExpired)
                }
                _ => ToolServiceError::InvalidEnvelope,
            }
        })?;
    encode_sabi_envelope(envelope).map_err(|_| ToolServiceError::InvalidEnvelope)?;
    let wire = context
        .caller
        .as_ref()
        .ok_or(ToolServiceError::BindingMismatch)?;
    let runtime = call.caller;
    if wire.principal_id != runtime.principal.as_bytes()
        || wire.application_id != runtime.application.as_bytes()
        || wire.process_id != runtime.process.as_bytes()
        || wire.process_generation != runtime.process_generation.get()
        || context.idempotency_key.as_slice()
            != call
                .idempotency_key
                .as_ref()
                .map_or(&[][..], |key| key.as_bytes())
        || context.deadline_monotonic_ns != call.deadline_monotonic_ns.unwrap_or(0)
    {
        return Err(ToolServiceError::BindingMismatch);
    }
    if context.task_execution_binding.is_some() {
        return Err(ToolServiceError::UnsupportedTaskBinding);
    }
    // Wire slots and reservation/activity hints do not provide authority.
    Ok(())
}

fn check_digest(envelope: &Envelope, digest: [u8; 32]) -> Result<(), ToolServiceError> {
    let Some(CommonContext::RequestContext(context)) = &envelope.common_context else {
        return Err(ToolServiceError::InvalidEnvelope);
    };
    if !context.proposal_or_input_digest_sha256.is_empty()
        && context.proposal_or_input_digest_sha256 != digest
    {
        return Err(ToolServiceError::DigestMismatch);
    }
    Ok(())
}

fn codec_error(error: &CompatibilityError) -> ToolServiceError {
    if matches!(error, CompatibilityError::FrameTooLarge { .. }) {
        ToolServiceError::FrameTooLarge
    } else {
        ToolServiceError::InvalidPayload
    }
}

fn wasm_target(action: &ServiceAction) -> Result<[u8; 32], ToolServiceError> {
    match action.target.object {
        ServiceObject::ToolWasm(hash) => Ok(hash),
        _ => Err(ToolServiceError::InvalidRegistry),
    }
}

fn host_stop_error(stop: HostStop) -> ToolServiceError {
    match stop {
        HostStop::Deadline => ToolFailure::Deadline.into(),
        HostStop::Cancelled => ToolFailure::Cancelled.into(),
        HostStop::StdoutLimit => ToolFailure::StdoutLimit.into(),
        HostStop::StderrLimit => ToolFailure::StderrLimit.into(),
        HostStop::Protocol => ToolFailure::HostLost.into(),
        HostStop::Control(error) => error.into(),
    }
}

fn before_dispatch_failure(error: ToolServiceError) -> ToolFailure {
    match error {
        ToolServiceError::Authorization(AuthorizationError::DeadlineExpired) => {
            ToolFailure::Deadline
        }
        _ => ToolFailure::Denied,
    }
}

/// Digest over the canonical list payload and registry identity.
///
/// # Panics
/// The fixed schema payload always encodes with the matching codec.
#[must_use]
pub fn list_input_digest(registration: [u8; 16]) -> [u8; 32] {
    let canonical = encode_list_tools_request(&ListToolsRequest {
        schema: Some(tool_sandbox_schema_identity()),
    })
    .expect("fixed list payload is bounded");
    digest_parts(b"list_tools", registration, &[&canonical])
}

/// Digest over the canonical invoke payload and registry identity.
///
/// # Errors
/// Rejects an unencodable or oversized canonical request.
pub fn invoke_input_digest(
    registration: [u8; 16],
    request: &InvokeToolRequest,
) -> Result<[u8; 32], ToolServiceError> {
    let canonical = encode_invoke_tool_request(&InvokeToolRequest {
        schema: Some(tool_sandbox_schema_identity()),
        arguments_json: request.arguments_json.clone(),
        tool: request.tool.clone(),
    })
    .map_err(|error| codec_error(&error))?;
    Ok(digest_parts(b"invoke_tool", registration, &[&canonical]))
}

fn digest_parts(domain: &[u8], registration: [u8; 16], parts: &[&[u8]]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"nlos/tool-sandbox/input/v1\0");
    hash.update((domain.len() as u64).to_le_bytes());
    hash.update(domain);
    hash.update(registration);
    for part in parts {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part);
    }
    hash.finalize().into()
}

fn control_input_digest(method: ServiceMethod, handle: ToolOperationHandle) -> [u8; 32] {
    let domain = match method {
        ServiceMethod::QueryToolOperation => b"query_operation".as_slice(),
        ServiceMethod::CancelToolOperation => b"cancel_operation".as_slice(),
        _ => unreachable!("internal tool operation control method"),
    };
    digest_parts(
        domain,
        handle.id.into_bytes(),
        &[&handle.generation.get().to_le_bytes()],
    )
}
