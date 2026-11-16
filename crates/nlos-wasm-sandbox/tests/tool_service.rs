use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use nlos_capability::CapabilityHandle;
use nlos_capability::service_call::{
    AuthorizationError, CallBinding, PolicyGrant, RuntimeCaller, ServiceAuthorizationBinding,
    ServiceAuthorizer, ServiceMethod, ServiceObject, ServiceTime,
};
use nlos_schema::sabi::v1::{
    CallerIdentity, Envelope, InvokeToolRequest, ListToolsRequest, SabiRequestContext,
    SchemaIdentity, TaskExecutionBinding, envelope::CommonContext,
};
use nlos_schema::{
    SABI_ENVELOPE_SCHEMA, decode_invoke_tool_result, decode_list_tools_result,
    encode_invoke_tool_request, encode_list_tools_request, tool_sandbox_schema_identity,
};
use nlos_types::{
    ApplicationId, CapabilityId, Generation, IdempotencyKey, NamespaceId, PrincipalId, ProcessId,
    SessionId,
};
use nlos_wasm_sandbox::tool_operation_store::{
    BeginDecision, JournalToolOperationStore, StoreError, ToolFailure, ToolHostQuota,
    ToolJournalLimits, ToolOperation, ToolOperationHandle, ToolOperationKey, ToolOperationSpec,
    ToolOperationState, ToolOperationStore,
};
use nlos_wasm_sandbox::tool_service::{
    INVOKE_TOOL, LIST_TOOLS, RegisteredTool, SERVICE_NAME, ToolRegistration, ToolSandboxService,
    ToolServiceClock, ToolServiceError, ToolServiceLimits, invoke_input_digest, list_input_digest,
};
use nlos_wasm_sandbox::{PreopenGrant, SandboxProfile};

const ROUTE: [u8; 16] = [9; 16];
const NAMESPACE: NamespaceId = NamespaceId::from_bytes([8; 16]);
const READ_ONLY: &str = r#"[{"name":"test","summary":"Test catalog","effect_class":"READ_ONLY","input_schema_json":"{}","unused":"private"}]"#;
const MUTATING: &str = r#"[{"name":"test","summary":"Test catalog","effect_class":"REVERSIBLE","input_schema_json":"{}","reconcile":"same-idempotency-key"}]"#;
const SIMPLE: &str = "(call $bytes (i32.const 1) (i32.const 2048) (i32.const 2)) (i32.const 0)";
static UNIQUE: AtomicU64 = AtomicU64::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "w82-tools-{}-{}",
            std::process::id(),
            UNIQUE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn artifact(&self) -> PathBuf {
        self.0.join("tool.wat")
    }
    fn journal(&self) -> PathBuf {
        self.0.join("journal")
    }
    fn workspace(&self) -> PathBuf {
        self.0.join("workspace")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn wat_string(bytes: &[u8]) -> String {
    let mut result = String::new();
    for byte in bytes {
        write!(result, "\\{byte:02x}").unwrap();
    }
    result
}

fn guest(catalog: &str, result: &[u8], body: &str) -> String {
    let catalog_data = wat_string(catalog.as_bytes());
    let result_data = wat_string(result);
    format!(
        r#"(module
      (import "wasi_snapshot_preview1" "fd_write" (func $write (param i32 i32 i32 i32) (result i32)))
      (import "wasi_snapshot_preview1" "path_open" (func $open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
      (import "wasi_snapshot_preview1" "fd_seek" (func $seek (param i32 i64 i32 i32) (result i32)))
      (memory (export "memory") 16)
      (data (i32.const 64) "started")
      (data (i32.const 2048) "{result_data}")
      (data (i32.const 524288) "{catalog_data}")
      (func $bytes (param $fd i32) (param $ptr i32) (param $len i32)
        (i32.store (i32.const 0) (local.get $ptr))
        (i32.store (i32.const 4) (local.get $len))
        (drop (call $write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 12))))
      (func $file (result i32)
        (if (call $open (i32.const 3) (i32.const 0) (i32.const 64) (i32.const 7)
                    (i32.const 1) (i64.const 64) (i64.const 0) (i32.const 0) (i32.const 24))
            (then unreachable))
        (i32.load (i32.const 24)))
      (func (export "__nlos_tool_catalog__") (result i32)
        (call $bytes (i32.const 1) (i32.const 524288) (i32.const {}))
        (i32.const 0))
      (func (export "run") (result i32) {body}))"#,
        catalog.len()
    )
}

fn write_guest(path: impl AsRef<Path>, catalog: &str, result: &[u8], body: &str) {
    std::fs::write(path, guest(catalog, result, body)).unwrap();
}

fn call(principal: u8, key: u8) -> CallBinding {
    CallBinding {
        caller: RuntimeCaller {
            principal: PrincipalId::from_bytes([principal; 16]),
            application: ApplicationId::from_bytes([2; 16]),
            process: ProcessId::from_bytes([3; 16]),
            process_generation: Generation::INITIAL,
            session: SessionId::from_bytes([4; 16]),
        },
        idempotency_key: Some(IdempotencyKey::from_bytes([key; 16])),
        deadline_monotonic_ns: Some(60_000_000_000),
        capabilities: vec![CapabilityHandle {
            capability_id: CapabilityId::from_bytes([5; 16]),
            generation: Generation::INITIAL,
        }],
    }
}

fn list(call: &CallBinding) -> Envelope {
    let request = ListToolsRequest {
        schema: Some(tool_sandbox_schema_identity()),
    };
    envelope(
        call,
        LIST_TOOLS,
        encode_list_tools_request(&request).unwrap(),
        list_input_digest(ROUTE),
    )
}

fn invoke(call: &CallBinding, name: &str, argument: &str) -> Envelope {
    let request = InvokeToolRequest {
        tool: name.to_owned(),
        arguments_json: argument.to_owned(),
        schema: Some(tool_sandbox_schema_identity()),
    };
    envelope(
        call,
        INVOKE_TOOL,
        encode_invoke_tool_request(&request).unwrap(),
        invoke_input_digest(ROUTE, &request).unwrap(),
    )
}

fn envelope(call: &CallBinding, method: &str, payload: Vec<u8>, digest: [u8; 32]) -> Envelope {
    let caller = call.caller;
    Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.into(),
            major: 1,
            ..SchemaIdentity::default()
        }),
        request_id: vec![77; 16],
        service: SERVICE_NAME.into(),
        method: method.into(),
        payload,
        common_context: Some(CommonContext::RequestContext(SabiRequestContext {
            caller: Some(CallerIdentity {
                principal_id: caller.principal.as_bytes().to_vec(),
                application_id: caller.application.as_bytes().to_vec(),
                process_id: caller.process.as_bytes().to_vec(),
                process_generation: caller.process_generation.get(),
            }),
            correlation_id: vec![77; 16],
            idempotency_key: call
                .idempotency_key
                .map_or(Vec::new(), |key| key.as_bytes().to_vec()),
            deadline_monotonic_ns: call.deadline_monotonic_ns.unwrap_or(0),
            proposal_or_input_digest_sha256: digest.to_vec(),
            ..SabiRequestContext::default()
        })),
    }
}

fn metadata(envelope: &mut Envelope) -> &mut SabiRequestContext {
    let Some(CommonContext::RequestContext(context)) = envelope.common_context.as_mut() else {
        panic!("missing context")
    };
    context
}

struct Policy<F>(F);
impl<F> ServiceAuthorizer for Policy<F>
where
    F: Fn(&ServiceAuthorizationBinding, ServiceTime) -> Result<PolicyGrant, AuthorizationError>
        + Send
        + Sync,
{
    fn authorize(
        &self,
        binding: &ServiceAuthorizationBinding,
        time: ServiceTime,
    ) -> Result<PolicyGrant, AuthorizationError> {
        (self.0)(binding, time)
    }
}

fn approve(
    binding: &ServiceAuthorizationBinding,
    now: ServiceTime,
) -> Result<PolicyGrant, AuthorizationError> {
    if binding.action.target.namespace != NAMESPACE {
        return Err(AuthorizationError::Denied);
    }
    Ok(PolicyGrant {
        binding: binding.clone(),
        capability: binding.call.capabilities[0],
        valid_from_ms: now.wall_ms,
        valid_until_ms: now.wall_ms,
    })
}

fn clock() -> Arc<ToolServiceClock> {
    let start = Instant::now();
    Arc::new(move || {
        Ok(ServiceTime {
            wall_ms: 100 + u64::try_from(start.elapsed().as_millis()).unwrap(),
            monotonic_ns: u64::try_from(start.elapsed().as_nanos()).unwrap(),
        })
    })
}

fn service(
    scratch: &Scratch,
    profile: SandboxProfile,
    limits: ToolServiceLimits,
    clock: Arc<ToolServiceClock>,
    policy: Arc<dyn ServiceAuthorizer>,
) -> ToolSandboxService {
    service_with_store(
        scratch,
        profile,
        limits,
        clock,
        policy,
        Box::new(
            JournalToolOperationStore::open(scratch.journal(), ToolJournalLimits::default())
                .unwrap(),
        ),
    )
}

fn service_with_store(
    scratch: &Scratch,
    profile: SandboxProfile,
    limits: ToolServiceLimits,
    clock: Arc<ToolServiceClock>,
    policy: Arc<dyn ServiceAuthorizer>,
    store: Box<dyn ToolOperationStore>,
) -> ToolSandboxService {
    try_service_with_store(scratch, profile, limits, clock, policy, store).unwrap()
}

fn try_service_with_store(
    scratch: &Scratch,
    profile: SandboxProfile,
    limits: ToolServiceLimits,
    clock: Arc<ToolServiceClock>,
    policy: Arc<dyn ServiceAuthorizer>,
    store: Box<dyn ToolOperationStore>,
) -> Result<ToolSandboxService, ToolServiceError> {
    ToolSandboxService::new(
        PathBuf::from(env!("CARGO_BIN_EXE_nlos-wasm-sandbox-host")),
        vec![ToolRegistration {
            id: ROUTE,
            namespace: NAMESPACE,
            generation: Generation::INITIAL,
            artifact: scratch.artifact(),
            tools: vec![RegisteredTool {
                name: "test".into(),
                entrypoint: "run".into(),
                profile,
            }],
        }],
        clock,
        policy,
        store,
        limits,
    )
}

fn normal(scratch: &Scratch) -> ToolSandboxService {
    service(
        scratch,
        SandboxProfile::deny_all(),
        ToolServiceLimits::default(),
        clock(),
        Arc::new(Policy(approve)),
    )
}

fn wire(operation: &ToolOperation) -> &[u8] {
    let ToolOperationState::Completed(result) = &operation.state else {
        panic!("{:?}", operation.state)
    };
    result.wire_bytes()
}

fn reason(operation: &ToolOperation) -> ToolFailure {
    let ToolOperationState::EffectUnknown { reason, .. } = operation.state else {
        panic!("{:?}", operation.state)
    };
    reason
}

#[test]
fn real_guest_catalog_and_durable_result_replay_after_reopen() {
    let scratch = Scratch::new();
    write_guest(scratch.artifact(), READ_ONLY, b"42", SIMPLE);
    let runtime = call(1, 1);
    let request = invoke(&runtime, "test", "{}");
    let sandbox = normal(&scratch);
    let first_list = sandbox
        .list_tools(ROUTE, &runtime, &list(&runtime))
        .unwrap();
    let tools = decode_list_tools_result(&first_list).unwrap();
    assert_eq!(tools.tools[0].summary, "Test catalog");
    assert!(!first_list.windows(7).any(|part| part == b"private"));
    let operation = sandbox.invoke_tool(ROUTE, &runtime, &request).unwrap();
    assert_eq!(
        decode_invoke_tool_result(wire(&operation))
            .unwrap()
            .output_json,
        "42"
    );
    drop(sandbox);

    let reopened = normal(&scratch);
    let mut reconnected = runtime.clone();
    reconnected.caller.session = SessionId::from_bytes([88; 16]);
    let replayed = reopened
        .replay(ROUTE, &reconnected, &invoke(&reconnected, "test", "{}"))
        .unwrap()
        .unwrap();
    assert_eq!(replayed, operation);
    assert_eq!(
        reopened.invoke_tool(ROUTE, &reconnected, &invoke(&reconnected, "test", "[]")),
        Err(StoreError::Conflict.into())
    );
    assert_eq!(
        reopened
            .cancel_operation(&runtime, operation.handle)
            .unwrap(),
        operation
    );
    assert_eq!(
        reopened.query_operation(&reconnected, operation.handle),
        Err(AuthorizationError::Denied.into())
    );

    let other = call(6, 1);
    let other_op = reopened
        .invoke_tool(ROUTE, &other, &invoke(&other, "test", "{}"))
        .unwrap();
    assert_ne!(other_op.handle, operation.handle);
    assert_eq!(
        reopened.query_operation(&other, operation.handle),
        Err(AuthorizationError::Denied.into())
    );
    assert_eq!(
        reopened.cancel_operation(&other, operation.handle),
        Err(AuthorizationError::Denied.into())
    );
    let mut stale = operation.handle;
    stale.generation = stale.generation.checked_next().unwrap();
    assert_eq!(
        reopened.cancel_operation(&runtime, stale),
        Err(AuthorizationError::Denied.into())
    );

    write_guest(
        scratch.artifact(),
        &READ_ONLY.replace("Test catalog", "Changed catalog"),
        b"42",
        SIMPLE,
    );
    let changed = reopened
        .list_tools(ROUTE, &runtime, &list(&runtime))
        .unwrap();
    assert_eq!(
        decode_list_tools_result(&changed).unwrap().tools[0].summary,
        "Changed catalog"
    );
    assert_eq!(
        reopened.invoke_tool(ROUTE, &runtime, &request),
        Err(StoreError::Conflict.into())
    );
}

#[test]
fn every_named_entry_rechecks_policy_and_trusted_clock() {
    let scratch = Scratch::new();
    write_guest(scratch.artifact(), READ_ONLY, b"42", SIMPLE);
    let enabled = Arc::new(AtomicBool::new(true));
    let policy_enabled = enabled.clone();
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let methods = calls.clone();
    let policy = Arc::new(Policy(move |binding: &ServiceAuthorizationBinding, now| {
        methods.lock().unwrap().push(binding.action.method);
        let mut grant = approve(binding, now)?;
        if !policy_enabled.load(Ordering::Acquire) {
            grant.valid_until_ms = now.wall_ms - 1;
            grant.valid_from_ms = grant.valid_until_ms;
        }
        Ok(grant)
    }));
    let clock_enabled = Arc::new(AtomicBool::new(true));
    let available = clock_enabled.clone();
    let server_clock = clock();
    let clock = Arc::new(move || {
        if available.load(Ordering::Acquire) {
            server_clock()
        } else {
            Err(AuthorizationError::TimeUnavailable)
        }
    });
    let sandbox = service(
        &scratch,
        SandboxProfile::deny_all(),
        ToolServiceLimits::default(),
        clock,
        policy,
    );
    let runtime = call(1, 2);
    let request = invoke(&runtime, "test", "{}");
    sandbox
        .list_tools(ROUTE, &runtime, &list(&runtime))
        .unwrap();
    let operation = sandbox.invoke_tool(ROUTE, &runtime, &request).unwrap();
    sandbox.query_operation(&runtime, operation.handle).unwrap();
    sandbox
        .cancel_operation(&runtime, operation.handle)
        .unwrap();
    sandbox.replay(ROUTE, &runtime, &request).unwrap();
    for method in [
        ServiceMethod::ListTools,
        ServiceMethod::InvokeTool,
        ServiceMethod::QueryToolOperation,
        ServiceMethod::CancelToolOperation,
    ] {
        assert!(calls.lock().unwrap().contains(&method));
    }

    enabled.store(false, Ordering::Release);
    let expired = ToolServiceError::Authorization(AuthorizationError::GrantExpired);
    assert_eq!(
        sandbox.list_tools(ROUTE, &runtime, &list(&runtime)),
        Err(expired)
    );
    assert_eq!(sandbox.invoke_tool(ROUTE, &runtime, &request), Err(expired));
    assert_eq!(sandbox.replay(ROUTE, &runtime, &request), Err(expired));
    assert_eq!(
        sandbox.query_operation(&runtime, operation.handle),
        Err(expired)
    );
    assert_eq!(
        sandbox.cancel_operation(&runtime, operation.handle),
        Err(expired)
    );

    clock_enabled.store(false, Ordering::Release);
    let unavailable = ToolServiceError::Authorization(AuthorizationError::TimeUnavailable);
    assert_eq!(
        sandbox.list_tools(ROUTE, &runtime, &list(&runtime)),
        Err(unavailable)
    );
    assert_eq!(
        sandbox.invoke_tool(ROUTE, &runtime, &request),
        Err(unavailable)
    );
    assert_eq!(sandbox.replay(ROUTE, &runtime, &request), Err(unavailable));
    assert_eq!(
        sandbox.query_operation(&runtime, operation.handle),
        Err(unavailable)
    );
    assert_eq!(
        sandbox.cancel_operation(&runtime, operation.handle),
        Err(unavailable)
    );
}

#[test]
fn forged_common_bindings_and_invalid_inputs_fail_before_guest() {
    let scratch = Scratch::new();
    // If any rejected request reaches launch, even catalog resolution fails.
    std::fs::write(scratch.artifact(), "not a module").unwrap();
    let sandbox = normal(&scratch);
    let runtime = call(1, 1);
    let request = invoke(&runtime, "test", "{}");
    for index in 0..4 {
        let mut forged = request.clone();
        let caller = metadata(&mut forged).caller.as_mut().unwrap();
        match index {
            0 => caller.principal_id[0] = 99,
            1 => caller.application_id[0] = 99,
            2 => caller.process_id[0] = 99,
            _ => caller.process_generation = 2,
        }
        assert_eq!(
            sandbox.invoke_tool(ROUTE, &runtime, &forged),
            Err(ToolServiceError::BindingMismatch)
        );
    }
    let mut forged = request.clone();
    metadata(&mut forged).proposal_or_input_digest_sha256[0] ^= 1;
    assert_eq!(
        sandbox.invoke_tool(ROUTE, &runtime, &forged),
        Err(ToolServiceError::DigestMismatch)
    );
    let mut forged = request.clone();
    metadata(&mut forged).task_execution_binding = Some(TaskExecutionBinding {
        task_attempt_id: vec![42; 16],
        task_authority_term: 1,
        isolation_domain_generation: 1,
        ..TaskExecutionBinding::default()
    });
    assert_eq!(
        sandbox.invoke_tool(ROUTE, &runtime, &forged),
        Err(ToolServiceError::UnsupportedTaskBinding)
    );
    assert_eq!(
        sandbox.invoke_tool(ROUTE, &runtime, &invoke(&runtime, "../../wat", "{}")),
        Err(ToolServiceError::UnknownTool)
    );
    let mut expired = runtime.clone();
    expired.deadline_monotonic_ns = Some(1);
    assert_eq!(
        sandbox.list_tools(ROUTE, &expired, &list(&expired)),
        Err(AuthorizationError::DeadlineExpired.into())
    );
    for invalid in [r#"{"x":1,"\u0078":2}"#, "[true,]", "1e+"] {
        assert_eq!(
            sandbox.invoke_tool(ROUTE, &runtime, &invoke(&runtime, "test", invalid)),
            Err(ToolServiceError::InvalidJson)
        );
    }
    let long = format!("{:8193}", "{}");
    assert_eq!(
        sandbox.invoke_tool(ROUTE, &runtime, &invoke(&runtime, "test", &long)),
        Err(ToolServiceError::InlineLimit)
    );
    let too_deep = format!("{}0{}", "[".repeat(65), "]".repeat(65));
    assert_eq!(
        sandbox.invoke_tool(ROUTE, &runtime, &invoke(&runtime, "test", &too_deep)),
        Err(ToolServiceError::InvalidJson)
    );
    assert_eq!(sandbox.replay(ROUTE, &runtime, &request).unwrap(), None);
}

#[test]
fn catalog_rejects_invalid_effects_extensions_and_unregistered_tools() {
    let scratch = Scratch::new();
    let runtime = call(1, 1);
    let sandbox = normal(&scratch);
    let too_many = format!(
        "[{}]",
        vec![READ_ONLY.trim_matches(['[', ']']); 129].join(",")
    );
    for catalog in [
        READ_ONLY.replace("READ_ONLY", "UNRECOGNIZED"),
        READ_ONLY.replace("READ_ONLY", "COMPENSATABLE"),
        MUTATING.replace("same-idempotency-key", ""),
        READ_ONLY.replace(
            "\"name\":\"test\"",
            "\"name\":\"test\",\"\\u006Eame\":\"test\"",
        ),
        READ_ONLY.replace("\"name\":\"test\"", "\"name\":\"guest-selected-export\""),
        too_many,
    ] {
        write_guest(scratch.artifact(), &catalog, b"42", SIMPLE);
        assert_eq!(
            sandbox.list_tools(ROUTE, &runtime, &list(&runtime)),
            Err(ToolServiceError::InvalidCatalog)
        );
    }
    write_guest(scratch.artifact(), "[]", b"42", SIMPLE);
    assert_eq!(
        sandbox.invoke_tool(ROUTE, &runtime, &invoke(&runtime, "test", "{}")),
        Err(ToolServiceError::UnknownTool)
    );
    let compensatable = MUTATING.replace("REVERSIBLE", "COMPENSATABLE").replace(
        "\"reconcile\"",
        "\"compensation_handler\":\"undo\",\"reconcile\"",
    );
    write_guest(scratch.artifact(), &compensatable, b"42", SIMPLE);
    let wire = sandbox
        .list_tools(ROUTE, &runtime, &list(&runtime))
        .unwrap();
    assert_eq!(
        decode_list_tools_result(&wire).unwrap().tools[0].compensation_handler,
        "undo"
    );
}

#[test]
fn actual_load_refuses_artifact_replaced_after_authorization_hash() {
    let scratch = Scratch::new();
    write_guest(scratch.artifact(), READ_ONLY, b"42", SIMPLE);
    let path = scratch.artifact();
    let calls = AtomicUsize::new(0);
    let policy = Arc::new(Policy(move |binding: &ServiceAuthorizationBinding, now| {
        assert!(matches!(
            binding.action.target.object,
            ServiceObject::ToolWasm(_)
        ));
        if binding.action.method == ServiceMethod::InvokeTool
            && calls.fetch_add(1, Ordering::Relaxed) == 3
        {
            write_guest(&path, READ_ONLY, b"99", SIMPLE);
        }
        approve(binding, now)
    }));
    let sandbox = service(
        &scratch,
        SandboxProfile::deny_all(),
        ToolServiceLimits::default(),
        clock(),
        policy,
    );
    let runtime = call(1, 1);
    sandbox
        .list_tools(ROUTE, &runtime, &list(&runtime))
        .unwrap();
    let result = sandbox
        .invoke_tool(ROUTE, &runtime, &invoke(&runtime, "test", "{}"))
        .unwrap();
    assert_eq!(reason(&result), ToolFailure::ArtifactChanged);
}

#[test]
fn measured_spawn_and_run() {
    let scratch = Scratch::new();
    write_guest(scratch.artifact(), READ_ONLY, b"42", SIMPLE);
    let sandbox = normal(&scratch);
    let runtime = call(1, 1);
    sandbox
        .list_tools(ROUTE, &runtime, &list(&runtime))
        .unwrap();
    let mut samples = Vec::new();
    for key in 1..=20 {
        let runtime = call(1, key);
        let request = invoke(&runtime, "test", "{}");
        let start = Instant::now();
        let operation = sandbox.invoke_tool(ROUTE, &runtime, &request).unwrap();
        assert_eq!(
            decode_invoke_tool_result(wire(&operation))
                .unwrap()
                .output_json,
            "42"
        );
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "platform={} arch={} guest=test argv={{}} samples={} fuel=1000000 memory_pages=16 epoch_ms=30000 catalog=warm path=invoke+spawn+run+durable_result p50_ms={:.2} p95_ms={:.2}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        samples.len(),
        samples[9],
        samples[18]
    );
}

const SPIN: &str = "(loop $spin (br $spin)) (i32.const 0)";
const MARK_AND_SPIN: &str = r"
    (call $bytes (call $file) (i32.const 64) (i32.const 1))
    (loop $spin (br $spin))
    (i32.const 0)
";

fn writable_profile(scratch: &Scratch) -> SandboxProfile {
    let root = scratch.workspace();
    std::fs::create_dir_all(&root).unwrap();
    SandboxProfile::deny_all()
        .with_fuel(u64::MAX)
        .with_preopen(PreopenGrant::new("/workspace", &root, true).unwrap())
}
fn wait_for_marker(marker: &Path) {
    let start = Instant::now();
    while !marker.is_file() {
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "guest did not start"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn cancel_before_durable_dispatch_prevents_host_from_running() {
    let scratch = Scratch::new();
    write_guest(scratch.artifact(), MUTATING, b"42", MARK_AND_SPIN);
    let (ready, entered) = std::sync::mpsc::channel();
    let release = Arc::new(std::sync::Barrier::new(2));
    let proceed = release.clone();
    let count = AtomicUsize::new(0);
    let policy = Arc::new(Policy(move |binding: &ServiceAuthorizationBinding, now| {
        if binding.action.method == ServiceMethod::InvokeTool
            && count.fetch_add(1, Ordering::Relaxed) == 2
        {
            ready.send(()).unwrap();
            proceed.wait();
        }
        approve(binding, now)
    }));
    let sandbox = service(
        &scratch,
        writable_profile(&scratch),
        ToolServiceLimits::default(),
        clock(),
        policy,
    );
    let runtime = call(1, 1);
    let request = invoke(&runtime, "test", "{}");
    sandbox
        .list_tools(ROUTE, &runtime, &list(&runtime))
        .unwrap();
    std::thread::scope(|scope| {
        let executor = scope.spawn(|| sandbox.invoke_tool(ROUTE, &runtime, &request));
        entered.recv_timeout(Duration::from_secs(8)).unwrap();
        let operation = sandbox.replay(ROUTE, &runtime, &request).unwrap().unwrap();
        assert_eq!(operation.state, ToolOperationState::Registered);
        let cancelled = sandbox
            .cancel_operation(&runtime, operation.handle)
            .unwrap();
        release.wait();
        assert_eq!(cancelled.state, ToolOperationState::CancelledBeforeEffect);
        assert_eq!(executor.join().unwrap().unwrap(), cancelled);
        assert!(!scratch.workspace().join("started").exists());
    });
}

#[test]
fn running_cancel_kills_real_host_and_preserves_uncertain_effect() {
    let scratch = Scratch::new();
    write_guest(scratch.artifact(), MUTATING, b"42", MARK_AND_SPIN);
    let sandbox = service(
        &scratch,
        writable_profile(&scratch),
        ToolServiceLimits::default(),
        clock(),
        Arc::new(Policy(approve)),
    );
    let runtime = call(1, 1);
    let request = invoke(&runtime, "test", "{}");
    sandbox
        .list_tools(ROUTE, &runtime, &list(&runtime))
        .unwrap();
    std::thread::scope(|scope| {
        let executor = scope.spawn(|| sandbox.invoke_tool(ROUTE, &runtime, &request));
        wait_for_marker(&scratch.workspace().join("started"));
        let duplicate = sandbox.invoke_tool(ROUTE, &runtime, &request).unwrap();
        assert_eq!(duplicate.state, ToolOperationState::Dispatched);
        let now = Instant::now();
        let cancelled = sandbox
            .cancel_operation(&runtime, duplicate.handle)
            .unwrap();
        assert!(
            now.elapsed() < Duration::from_secs(3),
            "cancel waited for guest deadline"
        );
        assert_eq!(cancelled.state, ToolOperationState::CancelRequested);
        let completed = executor.join().unwrap().unwrap();
        assert_eq!(reason(&completed), ToolFailure::Cancelled);
        assert_eq!(
            sandbox
                .cancel_operation(&runtime, duplicate.handle)
                .unwrap(),
            completed
        );
    });
}

#[test]
fn deadline_and_unchanged_or_unavailable_observations_preserve_unknown() {
    let scratch = Scratch::new();
    write_guest(scratch.artifact(), MUTATING, b"42", SPIN);
    let profile = writable_profile(&scratch).with_epoch_deadline(150, 10);
    std::fs::write(scratch.workspace().join("existing"), b"same").unwrap();
    let runtime = call(1, 1);
    let request = invoke(&runtime, "test", "{}");
    let sandbox = service(
        &scratch,
        profile.clone(),
        ToolServiceLimits::default(),
        clock(),
        Arc::new(Policy(approve)),
    );
    let operation = sandbox.invoke_tool(ROUTE, &runtime, &request).unwrap();
    assert_eq!(reason(&operation), ToolFailure::Deadline);
    let ToolOperationState::EffectUnknown { directory, .. } = operation.state else {
        unreachable!()
    };
    assert!(directory.before.is_some());
    assert_eq!(directory.before, directory.after);
    assert_eq!(
        sandbox.replay(ROUTE, &runtime, &request).unwrap(),
        Some(operation)
    );
    drop(sandbox);

    for limits in [
        ToolServiceLimits {
            reconciliation_entries: 0,
            ..ToolServiceLimits::default()
        },
        ToolServiceLimits {
            reconciliation_bytes: 3,
            ..ToolServiceLimits::default()
        },
    ] {
        let sandbox = service(
            &scratch,
            profile.clone(),
            limits,
            clock(),
            Arc::new(Policy(approve)),
        );
        let runtime = call(
            1,
            if limits.reconciliation_entries == 0 {
                2
            } else {
                3
            },
        );
        let operation = sandbox
            .invoke_tool(ROUTE, &runtime, &invoke(&runtime, "test", "{}"))
            .unwrap();
        let ToolOperationState::EffectUnknown { directory, .. } = operation.state else {
            unreachable!()
        };
        assert_eq!(directory.before, None);
        assert_eq!(directory.after, None);
    }
}

#[test]
fn independent_stdout_stderr_wire_and_write_boundaries() {
    let scratch = Scratch::new();
    let sandbox = service(
        &scratch,
        writable_profile(&scratch),
        ToolServiceLimits::default(),
        clock(),
        Arc::new(Policy(approve)),
    );
    for (key, (body, expected)) in [
        (
            r"(memory.fill (i32.const 2050) (i32.const 32) (i32.const 262143))
            (call $bytes (i32.const 1) (i32.const 2048) (i32.const 262145)) (i32.const 0)",
            ToolFailure::StdoutLimit,
        ),
        (
            r"(call $bytes (i32.const 2) (i32.const 2048) (i32.const 61441))
            (i32.const 0)",
            ToolFailure::StderrLimit,
        ),
        (
            r"(memory.fill (i32.const 2050) (i32.const 32) (i32.const 262142))
            (call $bytes (i32.const 1) (i32.const 2048) (i32.const 262144)) (i32.const 0)",
            ToolFailure::FrameTooLarge,
        ),
        (
            r"(local $fd i32) (local $writes i32)
            (local.set $fd (call $file))
            (local.set $writes (i32.const 8))
            (loop $repeat
                (call $bytes (local.get $fd) (i32.const 0) (i32.const 1048576))
                (local.set $writes (i32.sub (local.get $writes) (i32.const 1)))
                (br_if $repeat (local.get $writes)))
            (call $bytes (local.get $fd) (i32.const 64) (i32.const 1))
            (i32.const 0)",
            ToolFailure::FileWriteLimit,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        write_guest(scratch.artifact(), MUTATING, b"42", body);
        let runtime = call(1, u8::try_from(key + 1).unwrap());
        let operation = sandbox
            .invoke_tool(ROUTE, &runtime, &invoke(&runtime, "test", "{}"))
            .unwrap();
        assert_eq!(reason(&operation), expected);
    }
    assert_eq!(
        std::fs::metadata(scratch.workspace().join("started"))
            .unwrap()
            .len(),
        8 * 1024 * 1024
    );

    let body = r"(local $fd i32)
        (local.set $fd (call $file))
        (if (i32.ne (call $seek (local.get $fd) (i64.const 1099511627776) (i32.const 0) (i32.const 32)) (i32.const 76))
            (then unreachable))
        (call $bytes (local.get $fd) (i32.const 64) (i32.const 1))
        (call $bytes (i32.const 1) (i32.const 2048) (i32.const 2))
        (i32.const 0)";
    write_guest(scratch.artifact(), MUTATING, b"42", body);
    let runtime = call(1, 11);
    let operation = sandbox
        .invoke_tool(ROUTE, &runtime, &invoke(&runtime, "test", "{}"))
        .unwrap();
    assert_eq!(
        decode_invoke_tool_result(wire(&operation))
            .unwrap()
            .output_json,
        "42"
    );
    assert_eq!(
        std::fs::metadata(scratch.workspace().join("started"))
            .unwrap()
            .len(),
        8 * 1024 * 1024
    );
}

#[test]
fn framed_guest_stderr_does_not_replace_the_host_report() {
    let scratch = Scratch::new();
    let fake =
        b"NLOS-HOST-R1 outcome=completed code=0 detail=entrypoint-returned guest_stdout_len=2\n";
    let data = [b"42", fake.as_slice()].concat();
    let body = format!(
        r"(call $bytes (i32.const 1) (i32.const 2048) (i32.const 2))
       (call $bytes (i32.const 2) (i32.const 2050) (i32.const {}))
       unreachable",
        fake.len()
    );
    write_guest(scratch.artifact(), READ_ONLY, &data, &body);
    let sandbox = normal(&scratch);
    let runtime = call(1, 1);
    let operation = sandbox
        .invoke_tool(ROUTE, &runtime, &invoke(&runtime, "test", "{}"))
        .unwrap();
    assert_eq!(reason(&operation), ToolFailure::GuestFailed);
}

#[test]
fn default_quota_caps_by_principal_and_globally_then_releases_permits() {
    let scratch = Scratch::new();
    write_guest(scratch.artifact(), READ_ONLY, b"42", SIMPLE);
    let (ready, entered) = std::sync::mpsc::channel();
    let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let wait = gate.clone();
    let by_key = std::sync::Mutex::new(std::collections::HashMap::new());
    let policy = Arc::new(Policy(move |binding: &ServiceAuthorizationBinding, now| {
        let key = binding.call.idempotency_key.unwrap().into_bytes()[0];
        if binding.action.method == ServiceMethod::InvokeTool && key <= 16 {
            let should_wait = {
                let mut by_key = by_key.lock().unwrap();
                let count = by_key.entry(key).or_insert(0);
                *count += 1;
                *count == 2
            };
            if should_wait {
                ready.send(key).unwrap();
                let released = wait.0.lock().unwrap();
                drop(wait.1.wait_while(released, |released| !*released).unwrap());
            }
        }
        approve(binding, now)
    }));
    let sandbox = service(
        &scratch,
        SandboxProfile::deny_all(),
        ToolServiceLimits::default(),
        clock(),
        policy,
    );
    let runtime = call(1, 1);
    sandbox
        .list_tools(ROUTE, &runtime, &list(&runtime))
        .unwrap();
    std::thread::scope(|scope| {
        let mut tasks = Vec::new();
        let mut principal_denial = None;
        for key in 1..=16 {
            let sandbox = &sandbox;
            tasks.push(scope.spawn(move || {
                let runtime = call((key - 1) / 4 + 1, key);
                sandbox.invoke_tool(ROUTE, &runtime, &invoke(&runtime, "test", "{}"))
            }));
            if key == 4 {
                for _ in 0..4 {
                    entered.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                let runtime = call(1, 22);
                principal_denial =
                    Some(sandbox.invoke_tool(ROUTE, &runtime, &invoke(&runtime, "test", "{}")));
            }
        }
        for _ in 0..12 {
            entered.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        let runtime = call(5, 23);
        let global_denial = sandbox.invoke_tool(ROUTE, &runtime, &invoke(&runtime, "test", "{}"));
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        assert_eq!(
            principal_denial.unwrap(),
            Err(ToolServiceError::NoHostPermit)
        );
        assert_eq!(global_denial, Err(ToolServiceError::NoHostPermit));
        for task in tasks {
            assert_eq!(
                decode_invoke_tool_result(wire(&task.join().unwrap().unwrap()))
                    .unwrap()
                    .output_json,
                "42"
            );
        }
        let next = sandbox
            .invoke_tool(ROUTE, &runtime, &invoke(&runtime, "test", "{}"))
            .unwrap();
        assert_eq!(
            decode_invoke_tool_result(wire(&next)).unwrap().output_json,
            "42"
        );
    });
}

#[test]
fn service_harness_child() {
    let Some(path) = std::env::var_os("NLOS_W82_ORPHAN_FIXTURE") else {
        return;
    };
    let scratch = Scratch(PathBuf::from(path));
    let profile = writable_profile(&scratch).with_epoch_deadline(500, 10);
    let sandbox = service(
        &scratch,
        profile,
        ToolServiceLimits::default(),
        clock(),
        Arc::new(Policy(approve)),
    );
    let runtime = call(1, 1);
    let request = invoke(&runtime, "test", "{}");
    let result = sandbox.invoke_tool(ROUTE, &runtime, &request).unwrap();
    assert_eq!(reason(&result), ToolFailure::Deadline);
}

#[cfg(unix)]
fn native_processes() -> Vec<(u32, u32)> {
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,state="])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let parent_process_id = fields.next()?.parse().ok()?;
            (!fields.next()?.starts_with('Z')).then_some((pid, parent_process_id))
        })
        .collect()
}

#[cfg(windows)]
fn find_native_host_child(parent: u32) -> u32 {
    let query = format!(
        "(Get-CimInstance Win32_Process -Filter 'ParentProcessId = {parent} AND Name = \"nlos-wasm-sandbox-host.exe\"').ProcessId"
    );
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &query])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

#[cfg(unix)]
fn find_native_host_child(parent: u32) -> u32 {
    native_processes()
        .into_iter()
        .find(|&(_, ppid)| ppid == parent)
        .unwrap()
        .0
}

#[cfg(windows)]
fn native_process_alive(pid: u32) -> bool {
    let query =
        format!("exit [int](-not [bool](Get-Process -Id {pid} -ErrorAction SilentlyContinue))");
    std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &query])
        .status()
        .unwrap()
        .success()
}

#[cfg(unix)]
fn native_process_alive(pid: u32) -> bool {
    native_processes()
        .into_iter()
        .any(|(running, _)| running == pid)
}

#[cfg(any(windows, unix))]
#[test]
fn orphan_host_terminates_after_service_harness_is_killed() {
    use std::process::{Command, Stdio};
    let scratch = Scratch::new();
    write_guest(scratch.artifact(), MUTATING, b"42", MARK_AND_SPIN);
    let mut harness = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "service_harness_child"])
        .env("NLOS_W82_ORPHAN_FIXTURE", &scratch.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    wait_for_marker(&scratch.workspace().join("started"));
    let host = find_native_host_child(harness.id());
    assert!(native_process_alive(host));
    harness.kill().unwrap();
    harness.wait().unwrap();
    assert!(
        native_process_alive(host),
        "harness was not killed independently of the host"
    );
    while native_process_alive(host) {
        assert!(
            started.elapsed() < Duration::from_secs(7),
            "orphan host survived its epoch/watchdog bounds"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let sandbox = normal(&scratch);
    let runtime = call(1, 1);
    let request = invoke(&runtime, "test", "{}");
    let recovered = sandbox.replay(ROUTE, &runtime, &request).unwrap().unwrap();
    assert_eq!(reason(&recovered), ToolFailure::HostLost);
    assert_eq!(
        sandbox.invoke_tool(ROUTE, &runtime, &request).unwrap(),
        recovered
    );
}

#[test]
fn real_tool_epoch_interrupts_independently_of_parent_deadline_and_watchdog() {
    use nlos_wasm_sandbox::process::{HostProcessRequest, build_isolated_host_command};
    use sha2::{Digest, Sha256};

    let scratch = Scratch::new();
    let fixture_path = scratch.artifact();
    write_guest(&fixture_path, READ_ONLY, b"42", SPIN);
    // Match the registry's parent resolution while leaving the guest leaf
    // for the host to open without following links.
    let artifact = fixture_path
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap()
        .join(fixture_path.file_name().unwrap());
    let digest = Sha256::digest(std::fs::read(&artifact).unwrap());
    let mut hex_digest = String::new();
    for byte in digest {
        write!(hex_digest, "{byte:02x}").unwrap();
    }
    let host = HostProcessRequest::new(
        env!("CARGO_BIN_EXE_nlos-wasm-sandbox-host"),
        &artifact,
        SandboxProfile::deny_all()
            .with_fuel(u64::MAX)
            .with_epoch_deadline(30, 10),
    )
    .with_entrypoint("run");
    let mut command = build_isolated_host_command(&host).unwrap();
    command.args([
        "--tool-digest",
        &hex_digest,
        "--tool-limits",
        "262144,61440,8388608",
        "--tool-wall-ms",
        "3000",
    ]);
    let start = Instant::now();
    let output = command.output().unwrap();
    let elapsed = start.elapsed();
    let diagnostics = || {
        format!(
            "fixture path {fixture_path:?}, resolved path {artifact:?}, status {:?}, elapsed {elapsed:?}, stderr {:?}, stdout {:?}",
            output.status,
            String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(256)]),
            String::from_utf8_lossy(&output.stdout[..output.stdout.len().min(256)]),
        )
    };
    assert_eq!(output.status.code(), Some(133), "{}", diagnostics());
    assert!(
        output
            .stderr
            .windows(15)
            .any(|window| window == b"epoch-interrupt"),
        "{}",
        diagnostics()
    );
    assert!(elapsed < Duration::from_secs(3), "{}", diagnostics());
}

#[test]
fn grant_for_a_different_target_cannot_admit_a_host() {
    let scratch = Scratch::new();
    std::fs::write(scratch.artifact(), b"deliberately not executable").unwrap();
    let policy = Arc::new(Policy(|binding: &ServiceAuthorizationBinding, now| {
        let mut grant = approve(binding, now)?;
        grant.binding.action.target.object = ServiceObject::ToolWasm([99; 32]);
        Ok(grant)
    }));
    let sandbox = service(
        &scratch,
        SandboxProfile::deny_all(),
        ToolServiceLimits::default(),
        clock(),
        policy,
    );
    let runtime = call(1, 1);
    assert_eq!(
        sandbox.list_tools(ROUTE, &runtime, &list(&runtime)),
        Err(AuthorizationError::BindingMismatch.into())
    );
    assert_eq!(
        sandbox.invoke_tool(ROUTE, &runtime, &invoke(&runtime, "test", "{}")),
        Err(AuthorizationError::BindingMismatch.into())
    );
}

#[test]
fn complete_result_encoding_obeys_exact_wire_boundary() {
    use nlos_schema::encode_invoke_tool_result;
    use nlos_schema::sabi::v1::InvokeToolResult;

    let scratch = Scratch::new();
    let sandbox = normal(&scratch);
    // The fixture finds the payload string size accounting for protobuf
    // metadata, then emits those bytes from the real guest.
    let exact = (65_490..65_536)
        .find(|&size| {
            let result = InvokeToolResult {
                schema: Some(tool_sandbox_schema_identity()),
                output_json: format!("42{}", " ".repeat(size - 2)),
            };
            encode_invoke_tool_result(&result).is_ok_and(|wire| wire.len() == 65_536)
        })
        .unwrap();

    for (counter, size) in [exact, exact + 1].into_iter().enumerate() {
        let body = format!(
            r"(memory.fill (i32.const 2050) (i32.const 32) (i32.const {}))
        (call $bytes (i32.const 2) (i32.const 2048) (i32.const 61440))
        (call $bytes (i32.const 1) (i32.const 2048) (i32.const {size}))
        (i32.const 0)",
            size - 2
        );
        write_guest(scratch.artifact(), READ_ONLY, b"42", &body);
        let runtime = call(1, u8::try_from(counter + 1).unwrap());
        let operation = sandbox
            .invoke_tool(ROUTE, &runtime, &invoke(&runtime, "test", "{}"))
            .unwrap();
        if counter == 0 {
            assert_eq!(wire(&operation).len(), 65_536);
        } else {
            assert_eq!(reason(&operation), ToolFailure::FrameTooLarge);
        }
    }
}

#[test]
fn complete_catalog_encoding_rejects_an_oversize_directory() {
    let scratch = Scratch::new();
    let schema = format!(r#"{{\"description\":\"{}\"}}"#, "a".repeat(14_000));
    let tools: Vec<_> = (0..5)
        .map(|i| RegisteredTool {
            name: format!("tool{i}"),
            entrypoint: "run".into(),
            profile: SandboxProfile::deny_all(),
        })
        .collect();
    let descriptors = tools.iter().map(|tool| format!(
        r#"{{"name":"{}","summary":"descriptor","effect_class":"READ_ONLY","input_schema_json":"{schema}"}}"#,
        tool.name,
    )).collect::<Vec<_>>().join(",");
    write_guest(
        scratch.artifact(),
        &format!("[{descriptors}]"),
        b"42",
        SIMPLE,
    );

    let sandbox = ToolSandboxService::new(
        env!("CARGO_BIN_EXE_nlos-wasm-sandbox-host"),
        vec![ToolRegistration {
            id: ROUTE,
            namespace: NAMESPACE,
            generation: Generation::INITIAL,
            artifact: scratch.artifact(),
            tools,
        }],
        clock(),
        Arc::new(Policy(approve)),
        Box::new(
            JournalToolOperationStore::open(scratch.journal(), ToolJournalLimits::default())
                .unwrap(),
        ),
        ToolServiceLimits::default(),
    )
    .unwrap();
    let caller = call(1, 1);
    assert_eq!(
        sandbox.list_tools(ROUTE, &caller, &list(&caller)),
        Err(ToolServiceError::FrameTooLarge)
    );
}

struct SharedWriter {
    journal: Arc<JournalToolOperationStore>,
    before_completion: Option<Box<dyn Fn() + Send + Sync>>,
    unavailable: Arc<AtomicBool>,
}

impl SharedWriter {
    fn new(journal: Arc<JournalToolOperationStore>) -> Self {
        Self {
            journal,
            before_completion: None,
            unavailable: Arc::default(),
        }
    }
}

impl ToolOperationStore for SharedWriter {
    fn host_quota(&self) -> Arc<ToolHostQuota> {
        self.journal.host_quota()
    }

    fn find(
        &self,
        key: ToolOperationKey,
        digest: [u8; 32],
        spec: ToolOperationSpec,
    ) -> Result<Option<ToolOperation>, StoreError> {
        self.journal.find(key, digest, spec)
    }

    fn begin(
        &self,
        key: ToolOperationKey,
        digest: [u8; 32],
        spec: ToolOperationSpec,
    ) -> Result<BeginDecision, StoreError> {
        self.journal.begin(key, digest, spec)
    }

    fn mark_dispatched(
        &self,
        operation: &ToolOperation,
        before: Option<[u8; 32]>,
    ) -> Result<ToolOperation, StoreError> {
        self.journal.mark_dispatched(operation, before)
    }

    fn request_cancel(&self, operation: &ToolOperation) -> Result<ToolOperation, StoreError> {
        self.journal.request_cancel(operation)
    }

    fn complete(
        &self,
        operation: &ToolOperation,
        terminal: ToolOperationState,
    ) -> Result<ToolOperation, StoreError> {
        if matches!(terminal, ToolOperationState::Completed(_))
            && let Some(before_completion) = self.before_completion.as_ref()
        {
            before_completion();
        }
        self.journal.complete(operation, terminal)
    }

    fn query(&self, handle: ToolOperationHandle) -> Result<ToolOperation, StoreError> {
        if self.unavailable.load(Ordering::Acquire) {
            return Err(StoreError::Durability);
        }
        self.journal.query(handle)
    }
}

#[test]
fn shared_writer_cancellation_reaches_the_executing_instance() {
    let scratch = Scratch::new();
    write_guest(scratch.artifact(), MUTATING, b"42", MARK_AND_SPIN);
    let profile = writable_profile(&scratch).with_epoch_deadline(800, 10);
    let journal = Arc::new(
        JournalToolOperationStore::open(scratch.journal(), ToolJournalLimits::default()).unwrap(),
    );
    let clock = clock();
    let make_service = || {
        service_with_store(
            &scratch,
            profile.clone(),
            ToolServiceLimits::default(),
            clock.clone(),
            Arc::new(Policy(approve)),
            Box::new(SharedWriter::new(journal.clone())),
        )
    };
    let executor = make_service();
    let controller = make_service();
    let runtime = call(1, 1);
    let request = invoke(&runtime, "test", "{}");
    executor
        .list_tools(ROUTE, &runtime, &list(&runtime))
        .unwrap();
    let (done, outcome) = std::sync::mpsc::channel();
    let marker = scratch.workspace().join("started");

    std::thread::scope(|scope| {
        scope.spawn(|| {
            done.send(executor.invoke_tool(ROUTE, &runtime, &request))
                .unwrap();
        });
        wait_for_marker(&marker);
        let dispatch = controller
            .replay(ROUTE, &runtime, &request)
            .unwrap()
            .unwrap();
        assert_eq!(dispatch.state, ToolOperationState::Dispatched);
        let mut stale = dispatch.handle;
        stale.generation = stale.generation.checked_next().unwrap();
        assert_eq!(
            controller.cancel_operation(&runtime, stale),
            Err(AuthorizationError::Denied.into())
        );
        assert_eq!(journal.query(dispatch.handle).unwrap(), dispatch);

        let cancel_started = Instant::now();
        let requested = controller
            .cancel_operation(&runtime, dispatch.handle)
            .unwrap();
        assert_eq!(requested.state, ToolOperationState::CancelRequested);
        let prompt = outcome.recv_timeout(Duration::from_millis(500));
        if prompt.is_err() {
            // Reap the same host even when the regression is reproduced.
            executor
                .cancel_operation(&runtime, dispatch.handle)
                .unwrap();
            outcome
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap();
        }
        let terminal = prompt
            .expect("shared durable cancellation did not reach the running host within 500 ms")
            .unwrap();
        println!(
            "shared_writer_cancel_to_reaped_terminal_ms={}",
            cancel_started.elapsed().as_millis()
        );
        assert_eq!(reason(&terminal), ToolFailure::Cancelled);
        assert_eq!(journal.query(dispatch.handle).unwrap(), terminal);
        assert_eq!(
            controller
                .cancel_operation(&runtime, dispatch.handle)
                .unwrap(),
            terminal
        );
    });

    std::fs::remove_file(&marker).unwrap();
    let replay = controller
        .replay(ROUTE, &runtime, &request)
        .unwrap()
        .unwrap();
    assert_eq!(
        executor.invoke_tool(ROUTE, &runtime, &request).unwrap(),
        replay
    );
    assert_eq!(
        controller.invoke_tool(ROUTE, &runtime, &request).unwrap(),
        replay
    );
    assert!(
        !marker.exists(),
        "both replays must leave the invocation host stopped"
    );
}

#[test]
fn shared_writer_cancel_and_completion_keep_one_terminal_receipt() {
    let scratch = Scratch::new();
    write_guest(scratch.artifact(), READ_ONLY, b"42", SIMPLE);
    let journal = Arc::new(
        JournalToolOperationStore::open(scratch.journal(), ToolJournalLimits::default()).unwrap(),
    );
    let server_clock = clock();
    let (entered, awaiting_completion) = std::sync::mpsc::channel();
    let once = std::sync::Mutex::new(Some(entered));
    let (release, proceed) = std::sync::mpsc::channel();
    let proceed = std::sync::Mutex::new(proceed);
    let executor_store = SharedWriter {
        before_completion: Some(Box::new(move || {
            if let Some(entered) = once.lock().unwrap().take() {
                entered.send(()).unwrap();
                proceed
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
        })),
        ..SharedWriter::new(journal.clone())
    };
    let make_service = |adapter| {
        service_with_store(
            &scratch,
            SandboxProfile::deny_all(),
            ToolServiceLimits::default(),
            server_clock.clone(),
            Arc::new(Policy(approve)),
            Box::new(adapter),
        )
    };
    let executor = make_service(executor_store);
    let controller = make_service(SharedWriter::new(journal.clone()));
    let runtime = call(1, 1);
    let request = invoke(&runtime, "test", "{}");

    std::thread::scope(|scope| {
        let operation = scope.spawn(|| executor.invoke_tool(ROUTE, &runtime, &request));
        awaiting_completion
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        let cancel = controller
            .replay(ROUTE, &runtime, &request)
            .and_then(|pending| controller.cancel_operation(&runtime, pending.unwrap().handle));
        release.send(()).unwrap();
        assert_eq!(cancel.unwrap().state, ToolOperationState::CancelRequested);
        let terminal = operation.join().unwrap().unwrap();
        assert_eq!(reason(&terminal), ToolFailure::Cancelled);
        assert_eq!(
            controller
                .cancel_operation(&runtime, terminal.handle)
                .unwrap(),
            terminal
        );
    });

    let runtime = call(1, 2);
    let completed = executor
        .invoke_tool(ROUTE, &runtime, &invoke(&runtime, "test", "{}"))
        .unwrap();
    assert_eq!(
        decode_invoke_tool_result(wire(&completed))
            .unwrap()
            .output_json,
        "42"
    );
    assert_eq!(
        controller
            .cancel_operation(&runtime, completed.handle)
            .unwrap(),
        completed
    );
}

#[test]
fn failed_durable_observation_stops_host_without_claiming_cancellation() {
    let scratch = Scratch::new();
    write_guest(scratch.artifact(), MUTATING, b"42", MARK_AND_SPIN);
    let journal = Arc::new(
        JournalToolOperationStore::open(scratch.journal(), ToolJournalLimits::default()).unwrap(),
    );
    let adapter = SharedWriter::new(journal.clone());
    let unavailable = adapter.unavailable.clone();
    let executor = service_with_store(
        &scratch,
        writable_profile(&scratch).with_epoch_deadline(800, 10),
        ToolServiceLimits::default(),
        clock(),
        Arc::new(Policy(approve)),
        Box::new(adapter),
    );
    let runtime = call(1, 1);
    let request = invoke(&runtime, "test", "{}");
    executor
        .list_tools(ROUTE, &runtime, &list(&runtime))
        .unwrap();
    let marker = scratch.workspace().join("started");
    let (done, outcome) = std::sync::mpsc::channel();

    std::thread::scope(|scope| {
        scope.spawn(|| {
            done.send(executor.invoke_tool(ROUTE, &runtime, &request))
                .unwrap();
        });
        wait_for_marker(&marker);
        let dispatched = executor.replay(ROUTE, &runtime, &request).unwrap().unwrap();
        unavailable.store(true, Ordering::Release);
        assert_eq!(
            outcome.recv_timeout(Duration::from_millis(500)).unwrap(),
            Err(ToolServiceError::Store(StoreError::Durability)),
        );
        let terminal = journal.query(dispatched.handle).unwrap();
        assert_eq!(reason(&terminal), ToolFailure::HostLost);
    });
}

const MARK_AND_RETURN: &str = r"
    (call $bytes (call $file) (i32.const 64) (i32.const 1))
    (call $bytes (i32.const 1) (i32.const 2048) (i32.const 2))
    (i32.const 0)
";

#[test]
fn shared_writer_limits_active_hosts_globally_across_instances() {
    assert_shared_host_quota(1, 4, 2);
}

#[test]
fn shared_writer_limits_active_hosts_per_principal_across_instances() {
    assert_shared_host_quota(16, 1, 1);
}

fn assert_shared_host_quota(global: usize, per_principal: usize, caller_marker: u8) {
    let first_dir = Scratch::new();
    let peer_dir = Scratch::new();
    write_guest(first_dir.artifact(), MUTATING, b"42", MARK_AND_SPIN);
    write_guest(peer_dir.artifact(), MUTATING, b"42", MARK_AND_RETURN);
    let journal = Arc::new(
        JournalToolOperationStore::open(first_dir.journal(), ToolJournalLimits::default()).unwrap(),
    );
    let limits = ToolServiceLimits {
        active_hosts: global,
        active_hosts_per_principal: per_principal,
        ..ToolServiceLimits::default()
    };
    let server_clock = clock();
    let make_service = |scratch| {
        service_with_store(
            scratch,
            writable_profile(scratch).with_epoch_deadline(800, 10),
            limits,
            server_clock.clone(),
            Arc::new(Policy(approve)),
            Box::new(SharedWriter::new(journal.clone())),
        )
    };
    let first = make_service(&first_dir);
    let peer = make_service(&peer_dir);
    let cold = make_service(&peer_dir);
    let first_caller = call(1, 1);
    let peer_caller = call(caller_marker, 2);
    let first_request = invoke(&first_caller, "test", "{}");
    let peer_request = invoke(&peer_caller, "test", "{}");
    first
        .list_tools(ROUTE, &first_caller, &list(&first_caller))
        .unwrap();
    peer.list_tools(ROUTE, &peer_caller, &list(&peer_caller))
        .unwrap();
    let marker = peer_dir.workspace().join("started");

    let (blocked_invoke, blocked_catalog, spawned, admitted_other) = std::thread::scope(|scope| {
        let executor = scope.spawn(|| first.invoke_tool(ROUTE, &first_caller, &first_request));
        wait_for_marker(&first_dir.workspace().join("started"));
        let blocked_catalog = cold.list_tools(ROUTE, &peer_caller, &list(&peer_caller));
        let blocked_invoke = peer.invoke_tool(ROUTE, &peer_caller, &peer_request);
        let spawned = marker.exists();
        let admitted_other = (global > 1).then(|| {
            let other_caller = call(2, 3);
            let request = invoke(&other_caller, "test", "{}");
            peer.invoke_tool(ROUTE, &other_caller, &request)
        });
        let dispatched = first
            .replay(ROUTE, &first_caller, &first_request)
            .unwrap()
            .unwrap();
        assert_eq!(dispatched.state, ToolOperationState::Dispatched);
        peer.cancel_operation(&first_caller, dispatched.handle)
            .unwrap();
        assert_eq!(
            reason(&executor.join().unwrap().unwrap()),
            ToolFailure::Cancelled
        );
        (blocked_invoke, blocked_catalog, spawned, admitted_other)
    });

    println!(
        "shared_host_quota_global={global} per_principal={per_principal} extra_host_wrote_marker={spawned}"
    );
    assert_eq!(blocked_invoke, Err(ToolServiceError::NoHostPermit));
    assert_eq!(blocked_catalog, Err(ToolServiceError::NoHostPermit));
    assert!(!spawned, "rejected invoke must never run the guest");
    assert_eq!(
        peer.replay(ROUTE, &peer_caller, &peer_request).unwrap(),
        None
    );
    if let Some(admitted_other) = admitted_other {
        assert_eq!(
            decode_invoke_tool_result(wire(&admitted_other.unwrap()))
                .unwrap()
                .output_json,
            "42"
        );
        std::fs::remove_file(&marker).unwrap();
    }

    cold.list_tools(ROUTE, &peer_caller, &list(&peer_caller))
        .unwrap();
    let admitted = peer
        .invoke_tool(ROUTE, &peer_caller, &peer_request)
        .unwrap();
    assert_eq!(
        decode_invoke_tool_result(wire(&admitted))
            .unwrap()
            .output_json,
        "42"
    );
    assert!(marker.exists());
}

#[test]
fn shared_writer_rejects_conflicting_config_without_rebinding() {
    let scratch = Scratch::new();
    write_guest(scratch.artifact(), READ_ONLY, b"42", SIMPLE);
    let journal = Arc::new(
        JournalToolOperationStore::open(scratch.journal(), ToolJournalLimits::default()).unwrap(),
    );
    let server_clock = clock();
    let make = |profile, global, per_principal| {
        try_service_with_store(
            &scratch,
            profile,
            ToolServiceLimits {
                active_hosts: global,
                active_hosts_per_principal: per_principal,
                ..ToolServiceLimits::default()
            },
            server_clock.clone(),
            Arc::new(Policy(approve)),
            Box::new(SharedWriter::new(journal.clone())),
        )
    };

    assert!(matches!(
        make(SandboxProfile::deny_all().with_fuel(0), 1, 1),
        Err(ToolServiceError::InvalidRegistry)
    ));
    let first = make(SandboxProfile::deny_all(), 2, 2).unwrap();
    let matching = make(SandboxProfile::deny_all(), 2, 2).unwrap();
    for (global, per_principal) in [(1, 2), (2, 1), (3, 2), (2, 3)] {
        assert!(matches!(
            make(SandboxProfile::deny_all(), global, per_principal),
            Err(ToolServiceError::ConflictingHostLimits)
        ));
    }
    drop(first);
    drop(matching);
    assert!(matches!(
        make(SandboxProfile::deny_all(), 3, 3),
        Err(ToolServiceError::ConflictingHostLimits)
    ));
    assert!(make(SandboxProfile::deny_all(), 2, 2).is_ok());
}

#[test]
fn cold_catalog_occupies_shared_quota_and_releases_it_on_failure() {
    let cold_dir = Scratch::new();
    let peer_dir = Scratch::new();
    write_guest(cold_dir.artifact(), "{}", b"42", SIMPLE);
    write_guest(peer_dir.artifact(), READ_ONLY, b"42", SIMPLE);
    let journal = Arc::new(
        JournalToolOperationStore::open(cold_dir.journal(), ToolJournalLimits::default()).unwrap(),
    );
    let limits = ToolServiceLimits {
        active_hosts: 1,
        ..ToolServiceLimits::default()
    };
    let server_clock = clock();
    let (entered, admission) = std::sync::mpsc::channel();
    let (release, proceed) = std::sync::mpsc::channel();
    let proceed = std::sync::Mutex::new(proceed);
    let attempts = AtomicUsize::new(0);
    let policy = Arc::new(Policy(move |binding: &ServiceAuthorizationBinding, now| {
        if binding.action.method == ServiceMethod::ListTools
            && attempts.fetch_add(1, Ordering::Relaxed) == 1
        {
            // The cold list has already taken its permit immediately before
            // the host request's final authorization and spawn.
            entered.send(()).unwrap();
            proceed
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
        }
        approve(binding, now)
    }));
    let make = |scratch, policy| {
        service_with_store(
            scratch,
            SandboxProfile::deny_all(),
            limits,
            server_clock.clone(),
            policy,
            Box::new(SharedWriter::new(journal.clone())),
        )
    };
    let cold = make(&cold_dir, policy);
    let peer = make(&peer_dir, Arc::new(Policy(approve)));
    let caller = call(1, 1);
    peer.list_tools(ROUTE, &caller, &list(&caller)).unwrap();
    let request = invoke(&caller, "test", "{}");

    std::thread::scope(|scope| {
        let loading = scope.spawn(|| cold.list_tools(ROUTE, &caller, &list(&caller)));
        admission.recv_timeout(Duration::from_secs(5)).unwrap();
        let blocked = peer.invoke_tool(ROUTE, &caller, &request);
        release.send(()).unwrap();
        assert_eq!(blocked, Err(ToolServiceError::NoHostPermit));
        assert_eq!(
            loading.join().unwrap(),
            Err(ToolServiceError::InvalidCatalog)
        );
    });

    assert_eq!(peer.replay(ROUTE, &caller, &request).unwrap(), None);
    let admitted = peer.invoke_tool(ROUTE, &caller, &request).unwrap();
    assert_eq!(
        decode_invoke_tool_result(wire(&admitted))
            .unwrap()
            .output_json,
        "42"
    );
}
