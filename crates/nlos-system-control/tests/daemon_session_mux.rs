//! W82-A daemon session-mux acceptance suite: single-connection multi-round
//! serving, the five-service dispatch (`SystemControl` + `ChannelControl` +
//! `WaitControl` + `OperationControl` + `ServiceDirectory`) on one connection,
//! connection-idle fail-closed behavior, and the clock WAL write-transaction
//! regression under the lane's frozen accounting (wall-receipt row deltas
//! against a single-shot baseline — never a raw `-wal` size assertion).

#![cfg(feature = "daemon")]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nlos_channel::ChannelAuthority;
use nlos_schema::SABI_ENVELOPE_SCHEMA;
use nlos_schema::sabi::v1::{
    CallerIdentity, CapabilityHandle, Envelope, RetryDirective, SabiErrorCode, SabiRequestContext,
    SchemaIdentity, envelope,
};
use nlos_system_control::auth::wait_control::{DaemonWaitControl, WAIT_CONTROL_SERVICE};
use nlos_types::IdempotencyKey;
use nlos_wait::WaitAuthority;
use nlos_wait_control::payload;
use nlos_wait_control::{
    decode_inspect_wait_result, decode_list_waits_result, decode_register_wait_result,
    encode_inspect_wait_request, encode_list_waits_request, encode_register_wait_request,
    wait_control_schema_identity,
};

static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

/// Short-lived temp root (sockets must stay under the macOS `SUN_LEN`
/// bound).
struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-w82a-{label}-{}-{sequence}",
            std::process::id(),
        )))
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Endpoint path in the host OS form (short Unix socket path / local pipe
/// name), mirroring the `daemon_assembly` helpers.
#[cfg(unix)]
fn temp_path(label: &str, suffix: &str) -> PathBuf {
    let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "nlos-w82a-{label}-{suffix}-{}-{sequence}.sock",
        std::process::id(),
    ))
}

/// Envelope-level failure projection: asserts the frozen failure-envelope
/// invariants (payload cleared, no receipts, no operation, correlation echo)
/// and returns the typed code/retry pair.
fn failure_of(response: &Envelope) -> (SabiErrorCode, RetryDirective) {
    assert!(response.payload.is_empty(), "failure clears the payload");
    let envelope::CommonContext::ResponseContext(context) =
        response.common_context.as_ref().expect("response context")
    else {
        panic!("failure envelope must carry a response context");
    };
    assert!(context.operation.is_none());
    assert_eq!(
        context.receipts,
        [] as [nlos_schema::sabi::v1::ReceiptReference; 0]
    );
    let failure = context.failure.as_ref().expect("typed failure");
    (
        SabiErrorCode::try_from(failure.code).unwrap(),
        RetryDirective::try_from(failure.retry).unwrap(),
    )
}

/// Seeds one durable channel under `root` and returns (channel authority,
/// wait authority, channel id bytes).
fn seed_wait_authority(root: &Path) -> (std::sync::Arc<ChannelAuthority>, WaitAuthority, [u8; 16]) {
    let channel = std::sync::Arc::new(ChannelAuthority::open(root.join("channel")).unwrap());
    let decision = channel
        .create_channel(nlos_channel::CreateChannelRequest {
            capacity_bytes: 4_096,
            policy_digest: [0xAB; 32],
            idempotency_key: IdempotencyKey::from_bytes([0x77; 16]),
            created_at_ms: 1_000,
        })
        .unwrap();
    let record = decision.record();
    let channel_id = record.channel_id.as_bytes();
    let waits = WaitAuthority::open(root.join("waits"), std::sync::Arc::clone(&channel)).unwrap();
    (channel, waits, *channel_id)
}

fn wait_request_context(idempotency_key: [u8; 16]) -> SabiRequestContext {
    SabiRequestContext {
        caller: Some(CallerIdentity {
            principal_id: vec![0x31; 16],
            application_id: vec![0x32; 16],
            process_id: vec![0x33; 16],
            process_generation: 1,
        }),
        activity_context: Vec::new(),
        task_execution_binding: None,
        correlation_id: vec![0x34; 16],
        idempotency_key: idempotency_key.to_vec(),
        deadline_monotonic_ns: u64::MAX,
        capability_handles: vec![CapabilityHandle {
            slot: 9,
            generation: 1,
        }],
        reservation_handle: None,
        proposal_or_input_digest_sha256: Vec::new(),
    }
}

fn wait_envelope(method: &str, context: SabiRequestContext, payload_bytes: Vec<u8>) -> Envelope {
    Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: 1,
            minor: 1,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        request_id: vec![0x35; 16],
        service: WAIT_CONTROL_SERVICE.to_owned(),
        method: method.to_owned(),
        common_context: Some(envelope::CommonContext::RequestContext(context)),
        payload: payload_bytes,
    }
}

fn register_payload(channel_id: &[u8; 16], binding_seed: u8, key: [u8; 16]) -> Vec<u8> {
    encode_register_wait_request(&payload::RegisterWaitRequest {
        schema: Some(wait_control_schema_identity()),
        binding: vec![binding_seed; 16],
        channel_id: channel_id.to_vec(),
        target_sequence: 5,
        idempotency_key: key.to_vec(),
        registered_at_ms: 1_000,
    })
    .unwrap()
}

/// A `service_directory`/`negotiate` request envelope for `wait_control`
/// under the given schema name.
fn negotiate_wire(schema_name: &str) -> Envelope {
    use nlos_schema::sabi::v1::LocalTransportKind;
    let kind = if cfg!(unix) {
        LocalTransportKind::UnixSocket
    } else {
        LocalTransportKind::WindowsNamedPipe
    };
    Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: 1,
            minor: 1,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        request_id: vec![0x57; 16],
        service: "service_directory".to_owned(),
        method: "negotiate".to_owned(),
        common_context: Some(envelope::CommonContext::RequestContext(
            wait_request_context([0x58; 16]),
        )),
        payload: nlos_schema::encode_negotiate_service_request(
            &nlos_schema::sabi::v1::NegotiateServiceRequest {
                schema: Some(nlos_schema::service_directory_schema_identity()),
                service: WAIT_CONTROL_SERVICE.to_owned(),
                schema_name: schema_name.to_owned(),
                major: 1,
                minimum_minor: 0,
                required_feature_ids: Vec::new(),
                supported_transport_kinds: vec![kind.into()],
            },
        )
        .unwrap(),
    }
}

/// An `operation_control`/`query` request envelope for a nonexistent
/// operation (the typed-NotFound dispatch probe).
fn op_ctl_envelope() -> Envelope {
    use nlos_schema::{encode_query_operation_request, operation_control_schema_identity};
    Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: 1,
            minor: 1,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        request_id: vec![0x59; 16],
        service: "operation_control".to_owned(),
        method: "query".to_owned(),
        common_context: Some(envelope::CommonContext::RequestContext(
            wait_request_context([0x5A; 16]),
        )),
        payload: encode_query_operation_request(&nlos_schema::sabi::v1::QueryOperationRequest {
            schema: Some(operation_control_schema_identity()),
            operation: Some(nlos_schema::sabi::v1::OperationReference {
                operation_id: vec![0xEE; 16],
                generation: 1,
            }),
        })
        .unwrap(),
    }
}

#[test]
fn daemon_wait_control_plain_face_serves_register_list_inspect() {
    let root = TempRoot::new("wait-plain");
    fs::create_dir_all(&root.0).unwrap();
    let (_channel, waits, channel_id) = seed_wait_authority(&root.0);

    let service = DaemonWaitControl::plain(&std::sync::Arc::new(waits));
    let key = [0x51; 16];
    let register = wait_envelope(
        "register_wait",
        wait_request_context(key),
        register_payload(&channel_id, 0xA1, key),
    );
    let response = service.handle_for_ipc(&register, 10, 0);
    let result = decode_register_wait_result(&response.payload).unwrap();
    let record = result.record.expect("registered wait record");
    let wait_id: [u8; 16] = record.wait_id.clone().try_into().unwrap();

    let list = wait_envelope(
        "list_waits",
        wait_request_context(key),
        encode_list_waits_request(&payload::ListWaitsRequest {
            schema: Some(wait_control_schema_identity()),
            filter_channel_id: Vec::new(),
        })
        .unwrap(),
    );
    let response = service.handle_for_ipc(&list, 10, 0);
    let listed = decode_list_waits_result(&response.payload).unwrap();
    assert_eq!(listed.waits.len(), 1, "one durable wait registered");
    assert_eq!(listed.waits[0].wait_id, record.wait_id);

    let inspect = wait_envelope(
        "inspect_wait",
        wait_request_context(key),
        encode_inspect_wait_request(&payload::InspectWaitRequest {
            schema: Some(wait_control_schema_identity()),
            wait_id: wait_id.to_vec(),
        })
        .unwrap(),
    );
    let response = service.handle_for_ipc(&inspect, 10, 0);
    let inspected = decode_inspect_wait_result(&response.payload).unwrap();
    assert_eq!(
        inspected.record.expect("inspect record").wait_id,
        record.wait_id
    );

    // Unknown method keeps the frozen failure-envelope invariants.
    let bogus = wait_envelope("bogus", wait_request_context(key), Vec::new());
    let response = service.handle_for_ipc(&bogus, 10, 0);
    assert_eq!(
        failure_of(&response),
        (SabiErrorCode::NotSupported, RetryDirective::DoNotRetry)
    );

    // The principal-bound authenticated face is exercised end-to-end over a
    // real handshake in the socket-level suite below (VerifiedPrincipalHandshake
    // has no public constructor, so the arm cannot be driven at dispatch
    // level without fabricating a handshake).
    let _ = Duration::from_secs(0);
}

// ---------------------------------------------------------------------------
// OperationControl adapter (L-011): query/cancel over the durable
// SqliteOperationStore, typed failures for unknown methods and operations.
// ---------------------------------------------------------------------------
mod operation_control_suite {
    use super::*;
    use nlos_operation::OperationSpec;
    use nlos_runtime::FiberHandle;
    use nlos_schema::sabi::v1::{OperationLifecycleState, OperationReference};
    use nlos_schema::{
        decode_operation_status, encode_cancel_operation_request, encode_query_operation_request,
        operation_control_schema_identity,
    };
    use nlos_store::{IdempotencyScope, SqliteOperationStore};
    use nlos_system_control::auth::operation_control::{
        OPERATION_CONTROL_SERVICE, OperationControlService,
    };
    use nlos_types::{
        ApplicationId, CancellationScopeId, ExecutionFiberId, Generation, IdempotencyKey,
        OperationId,
    };

    fn op_envelope(method: &str, payload: Vec<u8>) -> Envelope {
        Envelope {
            schema: Some(SchemaIdentity {
                name: SABI_ENVELOPE_SCHEMA.to_owned(),
                major: 1,
                minor: 1,
                critical_extension_ids: Vec::new(),
                non_critical_extension_ids: Vec::new(),
            }),
            request_id: vec![0x45; 16],
            service: OPERATION_CONTROL_SERVICE.to_owned(),
            method: method.to_owned(),
            common_context: Some(envelope::CommonContext::RequestContext(
                super::wait_request_context([0x46; 16]),
            )),
            payload,
        }
    }

    fn reference(handle: nlos_operation::OperationHandle) -> OperationReference {
        OperationReference {
            operation_id: handle.operation_id.as_bytes().to_vec(),
            generation: handle.generation.get(),
        }
    }

    fn seeded_store(root: &Path) -> (SqliteOperationStore, nlos_operation::OperationHandle) {
        let store = SqliteOperationStore::open(root.join("operations.sqlite3")).unwrap();
        let spec = OperationSpec {
            operation_id: OperationId::from_bytes([0x61; 16]),
            generation: Generation::INITIAL,
            owner_fiber: FiberHandle {
                fiber_id: ExecutionFiberId::from_bytes([0x33; 16]),
                generation: Generation::INITIAL,
            },
            cancellation_scope_id: CancellationScopeId::from_bytes([0x51; 16]),
            cancellation_generation: Generation::INITIAL,
        };
        let decision = store
            .begin_idempotent_operation(
                &IdempotencyScope {
                    application_id: ApplicationId::from_bytes([0x32; 16]),
                    service: "operation".to_owned(),
                    method: "get".to_owned(),
                },
                IdempotencyKey::from_bytes([0x52; 16]),
                [0x53; 32],
                spec,
            )
            .unwrap();
        (store, decision.operation())
    }

    #[test]
    fn operation_control_serves_query_cancel_and_typed_failures() {
        let root = super::TempRoot::new("opctl");
        fs::create_dir_all(&root.0).unwrap();
        let (store, handle) = seeded_store(&root.0);
        let service = OperationControlService::new(&store);
        let epoch = store.inspect(handle).unwrap().cancel_epoch.get();

        // query: Registered at the seeded generation.
        let query = op_envelope(
            "query",
            encode_query_operation_request(&nlos_schema::sabi::v1::QueryOperationRequest {
                schema: Some(operation_control_schema_identity()),
                operation: Some(reference(handle)),
            })
            .unwrap(),
        );
        let response = service.handle_for_ipc(&query, 10, 0);
        let status = decode_operation_status(&response.payload).unwrap();
        assert_eq!(status.state, i32::from(OperationLifecycleState::Registered));
        assert_eq!(status.cancel_epoch, epoch);

        // cancel with the current epoch: the never-dispatched operation
        // completes as CancelledBeforeEffect carrying the deterministic
        // no-effect receipt (the store's cancel-before-effect fast path).
        let cancel = op_envelope(
            "cancel",
            encode_cancel_operation_request(&nlos_schema::sabi::v1::CancelOperationRequest {
                schema: Some(operation_control_schema_identity()),
                operation: Some(reference(handle)),
                expected_cancel_epoch: epoch,
            })
            .unwrap(),
        );
        let response = service.handle_for_ipc(&cancel, 10, 0);
        let status = decode_operation_status(&response.payload).unwrap();
        assert_eq!(
            status.state,
            i32::from(OperationLifecycleState::CancelledBeforeEffect)
        );
        assert_eq!(
            status.cancel_epoch,
            epoch + 1,
            "the cancel fence advanced once"
        );
        assert!(
            status.receipt.is_some(),
            "cancelled-before-effect carries the no-effect receipt"
        );

        // query again observes the durable terminal state.
        let response = service.handle_for_ipc(&query, 10, 0);
        let status = decode_operation_status(&response.payload).unwrap();
        assert_eq!(
            status.state,
            i32::from(OperationLifecycleState::CancelledBeforeEffect)
        );

        // unknown operation: typed NotFound envelope, invariants intact.
        let mut missing = reference(handle);
        missing.operation_id = vec![0xEE; 16];
        let query_missing = op_envelope(
            "query",
            encode_query_operation_request(&nlos_schema::sabi::v1::QueryOperationRequest {
                schema: Some(operation_control_schema_identity()),
                operation: Some(missing),
            })
            .unwrap(),
        );
        let response = service.handle_for_ipc(&query_missing, 10, 0);
        assert_eq!(
            super::failure_of(&response),
            (SabiErrorCode::NotFound, RetryDirective::DoNotRetry)
        );

        // unknown method: typed NotSupported envelope.
        let bogus = op_envelope("bogus", Vec::new());
        let response = service.handle_for_ipc(&bogus, 10, 0);
        assert_eq!(
            super::failure_of(&response),
            (SabiErrorCode::NotSupported, RetryDirective::DoNotRetry)
        );
    }
}

// ---------------------------------------------------------------------------
// ServiceDirectory adapter (L-011): negotiate over the daemon's five-service
// offering bound to both endpoints.
// ---------------------------------------------------------------------------
mod directory_suite {
    use super::*;
    use nlos_schema::sabi::v1::{
        LocalTransportKind, NegotiateServiceRequest, negotiate_service_response,
    };
    use nlos_schema::{
        SABI_WAIT_CONTROL_SCHEMA, decode_negotiate_service_response,
        encode_negotiate_service_request,
    };
    use nlos_system_control::auth::directory_control::{
        NEGOTIATE_METHOD, SERVICE_DIRECTORY_SERVICE, ServiceDirectorySurface, daemon_registrations,
    };

    fn negotiate_envelope(payload: Vec<u8>) -> Envelope {
        Envelope {
            schema: Some(SchemaIdentity {
                name: SABI_ENVELOPE_SCHEMA.to_owned(),
                major: 1,
                minor: 1,
                critical_extension_ids: Vec::new(),
                non_critical_extension_ids: Vec::new(),
            }),
            request_id: vec![0x55; 16],
            service: SERVICE_DIRECTORY_SERVICE.to_owned(),
            method: NEGOTIATE_METHOD.to_owned(),
            common_context: Some(envelope::CommonContext::RequestContext(
                super::wait_request_context([0x56; 16]),
            )),
            payload,
        }
    }

    fn negotiate_request(service: &str, schema_name: &str) -> Vec<u8> {
        encode_negotiate_service_request(&NegotiateServiceRequest {
            schema: Some(nlos_schema::service_directory_schema_identity()),
            service: service.to_owned(),
            schema_name: schema_name.to_owned(),
            major: 1,
            minimum_minor: 0,
            required_feature_ids: Vec::new(),
            supported_transport_kinds: vec![endpoint_kind().into()],
        })
        .unwrap()
    }

    #[cfg(unix)]
    fn endpoint_kind() -> LocalTransportKind {
        LocalTransportKind::UnixSocket
    }

    #[cfg(windows)]
    fn endpoint_kind() -> LocalTransportKind {
        LocalTransportKind::WindowsNamedPipe
    }

    #[test]
    fn directory_negotiate_binds_every_service_on_both_endpoints() {
        let auth = PathBuf::from("/tmp/nlos-w82a-auth.sock");
        let plain = PathBuf::from("/tmp/nlos-w82a-plain.sock");
        let surface =
            ServiceDirectorySurface::new(daemon_registrations(&auth, &plain).unwrap()).unwrap();

        for service in [
            "system_control",
            "channel",
            "wait_control",
            "operation_control",
            "service_directory",
        ] {
            let schema_name = if service == "wait_control" {
                SABI_WAIT_CONTROL_SCHEMA
            } else {
                // The offering rows for the other four services use their
                // frozen schema constants; negotiate with the same name.
                match service {
                    "system_control" => "nlos.sabi.SystemControl",
                    "channel" => "nlos.sabi.ChannelControl",
                    "operation_control" => "nlos.sabi.OperationControl",
                    _ => "nlos.sabi.ServiceDirectory",
                }
            };
            let response = surface.handle_for_ipc(
                &negotiate_envelope(negotiate_request(service, schema_name)),
                10,
                0,
            );
            let result = decode_negotiate_service_response(&response.payload).unwrap();
            match result.result.expect("negotiate result") {
                negotiate_service_response::Result::Binding(binding) => {
                    let candidate = binding.candidate.expect("candidate");
                    assert_eq!(candidate.service, service, "bound service");
                    let address = binding.endpoint.expect("endpoint").address;
                    assert!(
                        address == auth.to_string_lossy() || address == plain.to_string_lossy(),
                        "endpoint {address} is one of the daemon's two endpoints"
                    );
                }
                negotiate_service_response::Result::Error(error) => {
                    panic!("negotiate {service} failed: {error:?}");
                }
            }
        }

        // Unknown service: the typed DirectoryError rides the response.
        let response = surface.handle_for_ipc(
            &negotiate_envelope(negotiate_request("nope", SABI_WAIT_CONTROL_SCHEMA)),
            10,
            0,
        );
        let result = decode_negotiate_service_response(&response.payload).unwrap();
        assert!(matches!(
            result.result,
            Some(negotiate_service_response::Result::Error(_))
        ));

        // Unknown method: typed NotSupported failure envelope.
        let mut bogus = negotiate_envelope(Vec::new());
        bogus.method = "resolve".to_owned();
        let response = surface.handle_for_ipc(&bogus, 10, 0);
        assert_eq!(
            super::failure_of(&response),
            (SabiErrorCode::NotSupported, RetryDirective::DoNotRetry)
        );
    }
}

// ---------------------------------------------------------------------------
// Five-way dispatch (W82-A): one projection fans out to the three new
// adapters plus the existing channel/system control arms; the unknown
// service keeps the frozen failure envelope.
// ---------------------------------------------------------------------------
mod dispatch_suite {
    use super::*;
    use nlos_channel::ChannelAuthority;
    use nlos_clock::AuthorityClock;
    use nlos_commit_coordinator::{RecoveryWorkerHealth, RecoveryWorkerState};
    use nlos_schema::sabi::v1::{Envelope as WireEnvelope, negotiate_service_response};
    use nlos_schema::{
        SABI_WAIT_CONTROL_SCHEMA, decode_list_waits_result, decode_negotiate_service_response,
        encode_list_waits_request,
    };
    use nlos_store::SqliteOperationStore;
    use nlos_system_control::auth::directory_control::{
        ServiceDirectorySurface, daemon_registrations,
    };
    use nlos_system_control::auth::operation_control::OperationControlService;
    use nlos_system_control::auth::{
        serve_validated_with_control_services, wait_control::DaemonWaitControl,
    };
    use nlos_system_control::channel_control::{
        AllowAllChannelControlAuthorizer, ChannelControlService,
    };
    use nlos_system_control::control::ControlCommand;
    use nlos_system_control::{
        RecoveryHealthSource, RecoverySystemControl, SystemControlAuthorizer,
    };
    use nlos_task::SqliteTaskAuthority;

    struct HealthyWorker;

    impl RecoveryHealthSource for HealthyWorker {
        fn recovery_health(&self) -> RecoveryWorkerHealth {
            RecoveryWorkerHealth {
                state: RecoveryWorkerState::Running,
                completed_cycles: 1,
                ..RecoveryWorkerHealth::default()
            }
        }
    }

    struct AllowAllSystemControl;

    impl SystemControlAuthorizer for AllowAllSystemControl {
        fn authorize_get(
            &self,
            _: &SabiRequestContext,
            _: &nlos_schema::sabi::v1::GetSystemControlRequest,
        ) -> Result<(), &'static str> {
            Ok(())
        }

        fn authorize_submit(
            &self,
            _: &SabiRequestContext,
            _: &nlos_schema::sabi::v1::ControlCommand,
        ) -> Result<(), &'static str> {
            Ok(())
        }
    }

    #[test]
    fn five_way_dispatch_hits_every_mounted_service_and_keeps_unknown_failures_stable() {
        let root = super::TempRoot::new("dispatch");
        fs::create_dir_all(&root.0).unwrap();
        let clock = AuthorityClock::open(root.0.join("clock")).unwrap();
        let tasks = SqliteTaskAuthority::open(root.0.join("tasks.sqlite3")).unwrap();
        let channel = std::sync::Arc::new(ChannelAuthority::open(root.0.join("channel")).unwrap());
        let waits = nlos_wait::WaitAuthority::open(root.0.join("waits"), channel.clone()).unwrap();
        let operations = SqliteOperationStore::open(root.0.join("operations.sqlite3")).unwrap();
        let directory = ServiceDirectorySurface::new(
            daemon_registrations(&root.0.join("auth.sock"), &root.0.join("plain.sock")).unwrap(),
        )
        .unwrap();

        let control = RecoverySystemControl::new(&tasks, &HealthyWorker, &AllowAllSystemControl);
        let channel_service =
            ChannelControlService::new(channel.as_ref(), AllowAllChannelControlAuthorizer);
        let wait_face = DaemonWaitControl::plain(&std::sync::Arc::new(waits));
        let operation_face = OperationControlService::new(&operations);

        let dispatch = |request: &WireEnvelope| {
            serve_validated_with_control_services(
                &control,
                &channel_service,
                &wait_face,
                &operation_face,
                &directory,
                &clock,
                500,
                request,
            )
        };

        // system_control arm: the InspectHealth compile path still succeeds.
        let health =
            nlos_system_control::control::build_request_envelope(&ControlCommand::InspectHealth)
                .unwrap();
        let response = dispatch(&health);
        let envelope::CommonContext::ResponseContext(context) =
            response.common_context.as_ref().unwrap()
        else {
            panic!("response context");
        };
        assert!(
            context.failure.is_none(),
            "system_control arm still serves: {:?}",
            context.failure
        );

        // wait_control arm: list over the empty registry.
        let response = dispatch(&super::wait_envelope(
            "list_waits",
            super::wait_request_context([0x61; 16]),
            encode_list_waits_request(&payload::ListWaitsRequest {
                schema: Some(wait_control_schema_identity()),
                filter_channel_id: Vec::new(),
            })
            .unwrap(),
        ));
        let listed = decode_list_waits_result(&response.payload).unwrap();
        assert_eq!(
            listed.waits,
            [] as [nlos_wait_control::payload::WaitRecord; 0]
        );

        // operation_control arm: unknown operation → typed NotFound envelope.
        let operation_request = super::op_ctl_envelope();
        let response = dispatch(&operation_request);
        assert_eq!(
            super::failure_of(&response),
            (SabiErrorCode::NotFound, RetryDirective::DoNotRetry)
        );

        // service_directory arm: negotiate wait_control binds the endpoint.
        let response = dispatch(&super::negotiate_wire(SABI_WAIT_CONTROL_SCHEMA));
        let result = decode_negotiate_service_response(&response.payload).unwrap();
        assert!(matches!(
            result.result,
            Some(negotiate_service_response::Result::Binding(_))
        ));

        // unknown service: the frozen SystemControl failure envelope.
        let mut unknown = health.clone();
        unknown.service = "nope".to_owned();
        let response = dispatch(&unknown);
        assert_eq!(
            super::failure_of(&response),
            (SabiErrorCode::NotSupported, RetryDirective::DoNotRetry)
        );
    }
}

// ---------------------------------------------------------------------------
// Socket-level acceptance suite (W82-A gates, §E.6.1.8 W82-A row): single
// 1,000-round connections with zero disconnection, connection-idle
// fail-closed behavior, five-service interleaving on ONE connection, the
// clock WAL write-transaction accounting against a single-shot baseline,
// and principal-bound wait mutations over a real handshake.
//
// Unix-gated like the daemon_assembly round-trips (filesystem sockets and
// the Unix-only plain connect client); the Windows lane keeps its existing
// binary smoke — the multi-round wire bytes are platform-neutral.
// ---------------------------------------------------------------------------
#[cfg(unix)]
mod socket_suite {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    use nlos_identity::{BootstrapDecision, BootstrapPrincipalRequest, KeyPurpose};
    use nlos_ipc::handshake::transport::authenticated_connect;
    use nlos_ipc::unix::connect;
    use nlos_ipc::{LocalRpcClient, TransportConfig};
    use nlos_schema::sabi::v1::negotiate_service_response;
    use nlos_schema::{
        SABI_WAIT_CONTROL_SCHEMA, decode_list_waits_result, decode_negotiate_service_response,
        decode_register_wait_result, encode_list_waits_request,
    };
    use nlos_system_control::control::{ControlCommand, build_request_envelope};
    use nlos_system_control::daemon::{
        DaemonOptions, SystemControlDaemon, assemble, serve_authenticated_endpoint,
        serve_plain_endpoint,
    };
    use nlos_types::PrincipalId;
    use rusqlite::Connection;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;
    use tokio::task::JoinHandle;

    struct DaemonHarness {
        daemon: Arc<SystemControlDaemon>,
        stop: Arc<AtomicBool>,
        servers: Vec<JoinHandle<()>>,
        auth_socket: PathBuf,
        plain_socket: PathBuf,
    }

    impl DaemonHarness {
        fn stop(mut self) {
            self.stop.store(true, Ordering::Relaxed);
            for server in self.servers.drain(..) {
                server.abort();
            }
            self.daemon.stop_worker();
            self.daemon.stop_materialization();
        }

        fn wall_receipts(root: &Path) -> i64 {
            let connection =
                Connection::open(root.join("clock").join("clock-authority.db")).unwrap();
            connection
                .query_row("SELECT COUNT(*) FROM wall_receipts", [], |row| row.get(0))
                .unwrap()
        }
    }

    /// Boots a daemon with both endpoints bound and the given idle budget;
    /// spawns exactly the requested endpoint loops.
    fn start(
        label: &str,
        idle: Duration,
        identity_key_file: Option<&Path>,
        spawn_auth: bool,
        spawn_plain: bool,
    ) -> (DaemonHarness, TempRoot) {
        let root = TempRoot::new(label);
        fs::create_dir_all(&root.0).unwrap();
        let auth_socket = temp_path(label, "auth");
        let plain_socket = temp_path(label, "plain");
        let mut options = DaemonOptions::new(&root.0)
            .with_auth_socket(&auth_socket)
            .with_plain_socket(&plain_socket)
            .with_connection_idle_timeout(idle);
        if let Some(key_file) = identity_key_file {
            options = options.with_identity_key_file(key_file);
        }
        let (daemon, endpoints) =
            assemble(options, tokio::runtime::Handle::current()).expect("assemble daemon");
        let stop = Arc::new(AtomicBool::new(false));
        let mut servers = Vec::new();
        if spawn_auth {
            servers.push(tokio::spawn(serve_authenticated_endpoint(
                Arc::clone(&daemon),
                endpoints.listener_authenticated,
                Arc::clone(&stop),
            )));
        }
        if spawn_plain {
            servers.push(tokio::spawn(serve_plain_endpoint(
                Arc::clone(&daemon),
                endpoints.listener_plain,
                Arc::clone(&stop),
            )));
        }
        let _root_path = root.0.clone();
        (
            DaemonHarness {
                daemon,
                stop,
                servers,
                auth_socket,
                plain_socket,
            },
            root,
        )
        // note: caller keeps the TempRoot alive; harness.stop() retires loops
    }

    fn write_key_file(root: &Path, seed: u8) -> PathBuf {
        // The desktop-client key-file contract: 64 hex characters of the
        // Ed25519 SEED (not the public key).
        let mut hex_key = String::new();
        for byte in [seed; 32] {
            use std::fmt::Write as _;
            let _ = write!(hex_key, "{byte:02x}");
        }
        let path = root.join("client.key");
        fs::write(&path, hex_key).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    fn hex16(value: &str) -> [u8; 16] {
        let bytes = value.as_bytes();
        assert_eq!(bytes.len(), 32, "principal hex");
        let mut out = [0u8; 16];
        for (index, byte) in out.iter_mut().enumerate() {
            let hi = (bytes[2 * index] as char).to_digit(16).unwrap();
            let lo = (bytes[2 * index + 1] as char).to_digit(16).unwrap();
            *byte = u8::try_from(hi * 16 + lo).unwrap();
        }
        out
    }

    async fn plain_client(socket: &Path) -> LocalRpcClient<tokio::net::UnixStream> {
        try_plain_client(socket).await.expect("plain connect")
    }

    /// Fallible variant: a budget-rejected newcomer can surface the rejection
    /// at `connect()` (macOS `ENOTCONN` when the server closes before the
    /// client syscall returns) or at the first exchange (EOF) — the same
    /// rejection.
    async fn try_plain_client(
        socket: &Path,
    ) -> Result<LocalRpcClient<tokio::net::UnixStream>, nlos_ipc::IpcError> {
        let config = TransportConfig::default();
        let (stream, _) = connect(socket, config).await?;
        Ok(LocalRpcClient::new(stream, config))
    }

    async fn authenticated_client(
        socket: &Path,
        principal: PrincipalId,
        key: &SigningKey,
    ) -> LocalRpcClient<tokio::net::UnixStream> {
        let config = TransportConfig::default();
        let framed = authenticated_connect(socket, config, principal, |digest: &[u8; 32]| {
            Ok(key.sign(digest).to_bytes())
        })
        .await
        .expect("authenticated connect");
        LocalRpcClient::new(framed.into_inner(), config)
    }

    /// One unique-correlation `system_control` `InspectHealth` envelope (the
    /// command wall receipt is keyed by the correlation id, so every round
    /// issues a fresh durable receipt).
    fn unique_health_request(sequence: u16) -> nlos_schema::sabi::v1::ExchangeRequest {
        let mut envelope = build_request_envelope(&ControlCommand::InspectHealth).unwrap();
        let mut id = vec![0_u8; 16];
        id[0] = u8::try_from(sequence >> 8).unwrap();
        id[1] = u8::try_from(sequence & 0xFF).unwrap();
        envelope.request_id = id;
        if let Some(envelope::CommonContext::RequestContext(context)) =
            envelope.common_context.as_mut()
        {
            context.correlation_id.clone_from(&envelope.request_id);
        }
        nlos_schema::sabi::v1::ExchangeRequest {
            envelope: Some(envelope),
        }
    }

    fn unique_health_id(sequence: u16) -> Vec<u8> {
        let mut id = vec![0_u8; 16];
        id[0] = u8::try_from(sequence >> 8).unwrap();
        id[1] = u8::try_from(sequence & 0xFF).unwrap();
        id
    }

    /// The five-service round rotation for the long-haul tests: every round
    /// is a decodable typed response (envelope-level failures like the
    /// unknown-operation probe are still successful exchanges — the gate is
    /// zero DISCONNECTIONS, not zero typed failures).
    async fn five_service_round(client: &LocalRpcClient<tokio::net::UnixStream>, sequence: u16) {
        match sequence % 5 {
            0 => {
                let response = client
                    .exchange_validated(unique_health_request(sequence))
                    .await
                    .expect("system_control round");
                assert_eq!(response.envelope().request_id, unique_health_id(sequence));
            }
            1 => {
                let mut request = super::negotiate_wire(SABI_WAIT_CONTROL_SCHEMA);
                request.request_id = vec![0xB1; 16];
                let response = client
                    .exchange_validated(nlos_schema::sabi::v1::ExchangeRequest {
                        envelope: Some(request),
                    })
                    .await
                    .expect("directory round");
                let result =
                    decode_negotiate_service_response(&response.envelope().payload).unwrap();
                assert!(matches!(
                    result.result,
                    Some(negotiate_service_response::Result::Binding(_))
                ));
            }
            2 => {
                let response = client
                    .exchange_validated(nlos_schema::sabi::v1::ExchangeRequest {
                        envelope: Some(super::op_ctl_envelope()),
                    })
                    .await
                    .expect("operation_control round");
                // Typed NotFound envelope, exchange itself succeeded.
                assert_eq!(response.envelope().payload, [] as [u8; 0]);
            }
            3 => {
                let mut request = super::wait_envelope(
                    "list_waits",
                    super::wait_request_context([0xC1; 16]),
                    encode_list_waits_request(&payload::ListWaitsRequest {
                        schema: Some(wait_control_schema_identity()),
                        filter_channel_id: Vec::new(),
                    })
                    .unwrap(),
                );
                request.request_id = vec![0xC2; 16];
                let response = client
                    .exchange_validated(nlos_schema::sabi::v1::ExchangeRequest {
                        envelope: Some(request),
                    })
                    .await
                    .expect("wait_control round");
                decode_list_waits_result(&response.envelope().payload).unwrap();
            }
            _ => {
                let mut request = unique_health_request(sequence);
                // A second system_control shape with a fresh correlation.
                if let Some(envelope) = request.envelope.as_mut() {
                    let mut id = vec![0xD1_u8; 16];
                    id[15] = u8::try_from(sequence & 0xFF).unwrap();
                    envelope.request_id = id;
                    if let Some(envelope::CommonContext::RequestContext(context)) =
                        envelope.common_context.as_mut()
                    {
                        context.correlation_id.clone_from(&envelope.request_id);
                    }
                }
                client
                    .exchange_validated(request)
                    .await
                    .expect("second system_control shape");
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn plain_endpoint_1000_round_single_connection_zero_disconnection() {
        let (harness, _root) = start("long-plain", Duration::from_secs(30), None, false, true);
        let client = plain_client(&harness.plain_socket).await;
        for sequence in 0..1_000_u16 {
            five_service_round(&client, sequence).await;
        }
        drop(client);
        harness.stop();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn authenticated_endpoint_1000_round_single_connection_zero_disconnection() {
        let root = TempRoot::new("long-auth");
        fs::create_dir_all(&root.0).unwrap();
        let key_file = write_key_file(&root.0, 0x9A);
        let (harness, _kept) = start(
            "long-auth",
            Duration::from_secs(30),
            Some(&key_file),
            true,
            false,
        );
        let principal = harness
            .daemon
            .bootstrapped_principal_hex
            .as_deref()
            .map(str::to_owned)
            .expect("bootstrapped principal");
        let key = SigningKey::from_bytes(&[0x9A; 32]);
        let client = authenticated_client(
            &harness.auth_socket,
            PrincipalId::from_bytes(hex16(&principal)),
            &key,
        )
        .await;
        for sequence in 0..1_000_u16 {
            five_service_round(&client, sequence).await;
        }
        drop(client);
        harness.stop();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn idle_timeout_fail_closes_plain_connection_and_endpoint_survives() {
        let (harness, _root) = start("idle-plain", Duration::from_millis(300), None, false, true);
        let client = plain_client(&harness.plain_socket).await;
        client
            .exchange_validated(unique_health_request(1))
            .await
            .expect("first round");
        // Silent past the idle window: the daemon retires the connection.
        tokio::time::sleep(Duration::from_millis(800)).await;
        let started = Instant::now();
        let second = client.exchange_validated(unique_health_request(2)).await;
        assert!(second.is_err(), "idle connection is closed: {second:?}");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "close observed promptly (server-initiated), not via the 5s frame budget"
        );
        // The endpoint survives: a fresh connection serves immediately.
        let fresh = plain_client(&harness.plain_socket).await;
        fresh
            .exchange_validated(unique_health_request(3))
            .await
            .expect("fresh connection after idle expiry");
        harness.stop();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn idle_timeout_fail_closes_authenticated_connection() {
        let root = TempRoot::new("idle-auth");
        fs::create_dir_all(&root.0).unwrap();
        let key_file = write_key_file(&root.0, 0x9B);
        let (harness, _kept) = start(
            "idle-auth",
            Duration::from_millis(300),
            Some(&key_file),
            true,
            false,
        );
        let principal = harness
            .daemon
            .bootstrapped_principal_hex
            .as_deref()
            .map(str::to_owned)
            .expect("bootstrapped principal");
        let key = SigningKey::from_bytes(&[0x9B; 32]);
        let client = authenticated_client(
            &harness.auth_socket,
            PrincipalId::from_bytes(hex16(&principal)),
            &key,
        )
        .await;
        client
            .exchange_validated(unique_health_request(1))
            .await
            .expect("first round");
        tokio::time::sleep(Duration::from_millis(800)).await;
        let second = client.exchange_validated(unique_health_request(2)).await;
        assert!(second.is_err(), "idle authenticated connection is closed");
        // The handshake path also survives.
        let fresh = authenticated_client(
            &harness.auth_socket,
            PrincipalId::from_bytes(hex16(&principal)),
            &key,
        )
        .await;
        fresh
            .exchange_validated(unique_health_request(3))
            .await
            .expect("fresh authenticated connection after idle expiry");
        harness.stop();
    }

    // -- clock WAL write-transaction accounting (frozen W82-A gate) ------
    //
    // 口径 (frozen by maintainer ruling 2026-11-08): the WAL gate counts
    // durable WRITE TRANSACTIONS per 1,000 rounds via the wall_receipts row
    // delta on a second read-only connection — never a raw `-wal` size
    // assertion. Each system_control exchange issues exactly one command
    // wall receipt under its §25.3 correlation id; each authenticated
    // connection issues exactly one handshake receipt under its one-time
    // nonce key. wall_receipts is append-only (UPDATE/DELETE abort by
    // trigger); a REPLAYED correlation re-reads its original receipt without
    // inserting a row (its Immediate transaction appends no WAL frame). The
    // wal_autocheckpoint fix point lives in nlos-clock, which this lane
    // deliberately does not touch — recorded as out-of-lane.

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn wal_multi_round_connection_writes_one_receipt_per_round_plus_one_handshake() {
        let (harness, root) = start("wal-multi", Duration::from_secs(30), None, false, true);
        // Plain first: no handshake receipt, so delta == rounds exactly.
        tokio::time::sleep(Duration::from_millis(300)).await; // settle worker/driver first pass
        let baseline = DaemonHarness::wall_receipts(&root.0);
        let client = plain_client(&harness.plain_socket).await;
        for sequence in 0..1_000_u16 {
            client
                .exchange_validated(unique_health_request(sequence))
                .await
                .expect("round");
        }
        drop(client);
        let after = DaemonHarness::wall_receipts(&root.0);
        assert_eq!(
            after - baseline,
            1_000,
            "plain multi-round: exactly one durable write per round, no amplification"
        );
        harness.stop();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn wal_authenticated_multi_round_adds_exactly_one_handshake_receipt() {
        let root = TempRoot::new("wal-auth");
        fs::create_dir_all(&root.0).unwrap();
        let key_file = write_key_file(&root.0, 0x9C);
        let (harness, kept) = start(
            "wal-auth",
            Duration::from_secs(30),
            Some(&key_file),
            true,
            false,
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let baseline = DaemonHarness::wall_receipts(&kept.0);
        let principal = harness
            .daemon
            .bootstrapped_principal_hex
            .as_deref()
            .map(str::to_owned)
            .expect("bootstrapped principal");
        let key = SigningKey::from_bytes(&[0x9C; 32]);
        let client = authenticated_client(
            &harness.auth_socket,
            PrincipalId::from_bytes(hex16(&principal)),
            &key,
        )
        .await;
        for sequence in 0..1_000_u16 {
            client
                .exchange_validated(unique_health_request(sequence))
                .await
                .expect("round");
        }
        drop(client);
        let after = DaemonHarness::wall_receipts(&kept.0);
        assert_eq!(
            after - baseline,
            1_001,
            "authenticated multi-round: rounds + exactly one handshake receipt"
        );
        harness.stop();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn wal_single_shot_baseline_writes_two_receipts_per_connection() {
        let root = TempRoot::new("wal-single");
        fs::create_dir_all(&root.0).unwrap();
        let key_file = write_key_file(&root.0, 0x9D);
        let (harness, kept) = start(
            "wal-single",
            Duration::from_secs(30),
            Some(&key_file),
            true,
            false,
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let baseline = DaemonHarness::wall_receipts(&kept.0);
        let principal = harness
            .daemon
            .bootstrapped_principal_hex
            .as_deref()
            .map(str::to_owned)
            .expect("bootstrapped principal");
        let key = SigningKey::from_bytes(&[0x9D; 32]);
        for sequence in 0..1_000_u16 {
            // Single-shot posture (pre-W82-A): connect + handshake + ONE
            // exchange + drop, once per round.
            let client = authenticated_client(
                &harness.auth_socket,
                PrincipalId::from_bytes(hex16(&principal)),
                &key,
            )
            .await;
            client
                .exchange_validated(unique_health_request(sequence))
                .await
                .expect("single-shot round");
        }
        let after = DaemonHarness::wall_receipts(&kept.0);
        assert_eq!(
            after - baseline,
            2_000,
            "single-shot baseline: one handshake + one command receipt per connection"
        );
        // Frozen dual assertion note: the multi-round posture measured above
        // writes strictly fewer durable clock receipts than this single-shot
        // baseline for the same 1,000 logical rounds (1,001 < 2,000 — it
        // halves the L-008 handshake write amplification). The comparison is
        // between the two measured suites, each pinned exactly above.
        harness.stop();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[allow(clippy::too_many_lines)] // One interleaved five-service scenario per pass, twice.
    async fn five_services_interleave_on_one_authenticated_connection() {
        use nlos_operation::OperationSpec;
        use nlos_runtime::FiberHandle;
        use nlos_types::{
            ApplicationId, CancellationScopeId, ExecutionFiberId, Generation, IdempotencyKey,
            OperationId,
        };

        let root = TempRoot::new("interleave");
        fs::create_dir_all(&root.0).unwrap();
        let key_file = write_key_file(&root.0, 0x9E);
        let (harness, _kept) = start(
            "interleave",
            Duration::from_secs(30),
            Some(&key_file),
            true,
            false,
        );
        let principal = harness
            .daemon
            .bootstrapped_principal_hex
            .as_deref()
            .map(str::to_owned)
            .expect("bootstrapped principal");
        let key = SigningKey::from_bytes(&[0x9E; 32]);
        let client = authenticated_client(
            &harness.auth_socket,
            PrincipalId::from_bytes(hex16(&principal)),
            &key,
        )
        .await;

        // Seed one operation directly through the daemon's own store, then
        // drive every service over the SAME connection, interleaved.
        let spec = OperationSpec {
            operation_id: OperationId::from_bytes([0x71; 16]),
            generation: Generation::INITIAL,
            owner_fiber: FiberHandle {
                fiber_id: ExecutionFiberId::from_bytes([0x72; 16]),
                generation: Generation::INITIAL,
            },
            cancellation_scope_id: CancellationScopeId::from_bytes([0x73; 16]),
            cancellation_generation: Generation::INITIAL,
        };
        let operation_handle = harness
            .daemon
            .operations
            .begin_idempotent_operation(
                &nlos_store::IdempotencyScope {
                    application_id: ApplicationId::from_bytes([0x74; 16]),
                    service: "operation".to_owned(),
                    method: "get".to_owned(),
                },
                IdempotencyKey::from_bytes([0x75; 16]),
                [0x76; 32],
                spec,
            )
            .unwrap()
            .operation();
        let channel_record = harness
            .daemon
            .channel
            .create_channel(nlos_channel::CreateChannelRequest {
                capacity_bytes: 4_096,
                policy_digest: [0x77; 32],
                idempotency_key: IdempotencyKey::from_bytes([0x78; 16]),
                created_at_ms: 1_000,
            })
            .unwrap()
            .record();
        let seeded_channel = channel_record.channel_id.as_bytes();

        let exchange =
            |request: nlos_schema::sabi::v1::ExchangeRequest| client.exchange_validated(request);

        for pass in 0..2_u8 {
            // system_control
            let mut health = build_request_envelope(&ControlCommand::InspectHealth).unwrap();
            health.request_id = vec![0xE0 + pass; 16];
            exchange(nlos_schema::sabi::v1::ExchangeRequest {
                envelope: Some(health),
            })
            .await
            .expect("system_control interleave");

            // channel: a real create per pass over the same connection
            // (unique key), positively decoded.
            {
                use nlos_system_control::channel_control::{
                    CHANNEL_CONTROL_SERVICE, CREATE_CHANNEL_METHOD,
                    channel_control_schema_identity, decode_create_channel_result,
                    encode_create_channel_request,
                };
                let key = [0xF8_u8 + pass; 16];
                let mut request = super::wait_envelope(
                    CREATE_CHANNEL_METHOD,
                    super::wait_request_context(key),
                    encode_create_channel_request(
                        &nlos_schema::sabi::v1::CreateChannelRequestWire {
                            schema: Some(channel_control_schema_identity()),
                            capacity_bytes: 128,
                            policy_digest: vec![0x44; 32],
                            idempotency_key: key.to_vec(),
                            created_at_ms: 2_000,
                        },
                    )
                    .unwrap(),
                );
                request.service = CHANNEL_CONTROL_SERVICE.to_owned();
                request.request_id = key.to_vec();
                let response = exchange(nlos_schema::sabi::v1::ExchangeRequest {
                    envelope: Some(request),
                })
                .await
                .expect("channel interleave");
                decode_create_channel_result(&response.envelope().payload)
                    .unwrap()
                    .record
                    .expect("channel created over the wire");
            }

            // service_directory: negotiate wait_control
            let mut negotiate = super::negotiate_wire(SABI_WAIT_CONTROL_SCHEMA);
            negotiate.request_id = vec![0xE2 + pass; 16];
            let response = exchange(nlos_schema::sabi::v1::ExchangeRequest {
                envelope: Some(negotiate),
            })
            .await
            .unwrap();
            let result = decode_negotiate_service_response(&response.envelope().payload).unwrap();
            assert!(matches!(
                result.result,
                Some(negotiate_service_response::Result::Binding(_))
            ));

            // wait_control: register against the seeded channel (unique key
            // per pass so both registrations are real writes).
            let wait_key = [0xF0 + pass; 16];
            let mut register = super::wait_envelope(
                "register_wait",
                super::wait_request_context(wait_key),
                super::register_payload(seeded_channel, 0xA0 + pass, wait_key),
            );
            register.request_id = vec![0xE4 + pass; 16];
            let response = exchange(nlos_schema::sabi::v1::ExchangeRequest {
                envelope: Some(register),
            })
            .await
            .unwrap();
            let registered = decode_register_wait_result(&response.envelope().payload).unwrap();
            let wait_id: [u8; 16] = registered
                .record
                .expect("registered wait")
                .wait_id
                .try_into()
                .unwrap();

            // operation_control: query → Registered, cancel → terminal.
            let query = {
                use nlos_schema::{
                    encode_query_operation_request, operation_control_schema_identity,
                };
                let mut envelope = super::op_ctl_envelope();
                envelope.request_id = vec![0xE6 + pass; 16];
                envelope.payload =
                    encode_query_operation_request(&nlos_schema::sabi::v1::QueryOperationRequest {
                        schema: Some(operation_control_schema_identity()),
                        operation: Some(nlos_schema::sabi::v1::OperationReference {
                            operation_id: operation_handle.operation_id.as_bytes().to_vec(),
                            generation: operation_handle.generation.get(),
                        }),
                    })
                    .unwrap();
                envelope
            };
            let response = exchange(nlos_schema::sabi::v1::ExchangeRequest {
                envelope: Some(query),
            })
            .await
            .unwrap();
            let status =
                nlos_schema::decode_operation_status(&response.envelope().payload).unwrap();
            if pass == 0 {
                assert_eq!(
                    status.state,
                    i32::from(nlos_schema::sabi::v1::OperationLifecycleState::Registered),
                    "pass 0: seeded operation queries Registered before cancel"
                );
                assert_eq!(status.receipt, None, "no receipt in Registered state");

                // cancel over the wire: the never-dispatched operation
                // completes as CancelledBeforeEffect carrying the
                // deterministic no-effect receipt.
                let cancel = {
                    use nlos_schema::{
                        encode_cancel_operation_request, operation_control_schema_identity,
                    };
                    let mut envelope = super::op_ctl_envelope();
                    envelope.method = "cancel".to_owned();
                    envelope.request_id = vec![0xE7; 16];
                    envelope.payload = encode_cancel_operation_request(
                        &nlos_schema::sabi::v1::CancelOperationRequest {
                            schema: Some(operation_control_schema_identity()),
                            operation: Some(nlos_schema::sabi::v1::OperationReference {
                                operation_id: operation_handle.operation_id.as_bytes().to_vec(),
                                generation: operation_handle.generation.get(),
                            }),
                            expected_cancel_epoch: status.cancel_epoch,
                        },
                    )
                    .unwrap();
                    envelope
                };
                let response = exchange(nlos_schema::sabi::v1::ExchangeRequest {
                    envelope: Some(cancel),
                })
                .await
                .unwrap();
                let cancelled =
                    nlos_schema::decode_operation_status(&response.envelope().payload).unwrap();
                assert_eq!(
                    cancelled.state,
                    i32::from(
                        nlos_schema::sabi::v1::OperationLifecycleState::CancelledBeforeEffect
                    ),
                    "cancel completes the never-dispatched operation"
                );
                assert!(cancelled.receipt.is_some(), "no-effect receipt attached");
            } else {
                assert_eq!(
                    status.state,
                    i32::from(
                        nlos_schema::sabi::v1::OperationLifecycleState::CancelledBeforeEffect
                    ),
                    "pass 1: the terminal state is durable"
                );
            }

            let _ = wait_id;
        }

        drop(client);
        harness.stop();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[allow(clippy::too_many_lines)] // Two-principal scenario across three connections.
    async fn authenticated_wait_face_binds_mutation_keys_to_the_verified_principal() {
        let root = TempRoot::new("principal");
        fs::create_dir_all(&root.0).unwrap();
        let key_file_a = write_key_file(&root.0, 0xAA);
        let (harness, _kept) = start(
            "principal",
            Duration::from_secs(30),
            Some(&key_file_a),
            true,
            false,
        );
        let principal_a = PrincipalId::from_bytes(hex16(
            &harness
                .daemon
                .bootstrapped_principal_hex
                .clone()
                .expect("principal a"),
        ));
        let key_a = SigningKey::from_bytes(&[0xAA; 32]);

        // Bootstrap a SECOND principal directly through the daemon's
        // identity authority (the same request shape the daemon bootstrap
        // uses), so two distinct verified principals can connect.
        let key_b = SigningKey::from_bytes(&[0xBB; 32]);
        let BootstrapDecision::Created(binding_b) = harness
            .daemon
            .identity
            .bootstrap_principal(BootstrapPrincipalRequest {
                principal_profile_digest: [0xB1; 32],
                control_domain_policy_digest: [0xB2; 32],
                public_key: key_b.verifying_key().to_bytes(),
                key_purpose: KeyPurpose::SemanticSigning,
                key_valid_from_ms: 0,
                key_valid_until_ms: 4_102_444_800_000,
                idempotency_key: IdempotencyKey::from_bytes([0xB3; 16]),
                created_at_ms: 0,
            })
            .unwrap()
        else {
            unreachable!("fresh principal bootstraps");
        };

        let channel_record = harness
            .daemon
            .channel
            .create_channel(nlos_channel::CreateChannelRequest {
                capacity_bytes: 4_096,
                policy_digest: [0x77; 32],
                idempotency_key: IdempotencyKey::from_bytes([0x88; 16]),
                created_at_ms: 1_000,
            })
            .unwrap()
            .record();
        let seeded_channel = channel_record.channel_id.as_bytes();

        // Both principals send the SAME register_wait bytes (same key, same
        // binding, same channel) over their own authenticated connections.
        let shared_key = [0xCA; 16];
        let register_request = |request_id: u8| {
            let mut envelope = super::wait_envelope(
                "register_wait",
                super::wait_request_context(shared_key),
                super::register_payload(seeded_channel, 0xD0, shared_key),
            );
            envelope.request_id = vec![request_id; 16];
            nlos_schema::sabi::v1::ExchangeRequest {
                envelope: Some(envelope),
            }
        };

        let client_a = authenticated_client(&harness.auth_socket, principal_a, &key_a).await;
        let response = client_a
            .exchange_validated(register_request(1))
            .await
            .unwrap();
        let registered_a = decode_register_wait_result(&response.envelope().payload).unwrap();
        let wait_a: [u8; 16] = registered_a
            .record
            .expect("principal a registered")
            .wait_id
            .try_into()
            .unwrap();

        // Same principal + same key ⇒ durable replay (same wait id).
        let response = client_a
            .exchange_validated(register_request(2))
            .await
            .unwrap();
        let replay = decode_register_wait_result(&response.envelope().payload).unwrap();
        assert!(replay.replayed, "same principal replays its own receipt");
        assert_eq!(
            replay.record.expect("replayed record").wait_id,
            wait_a.to_vec()
        );

        let client_b =
            authenticated_client(&harness.auth_socket, binding_b.principal_id, &key_b).await;
        let response = client_b
            .exchange_validated(register_request(3))
            .await
            .unwrap();
        let registered_b = decode_register_wait_result(&response.envelope().payload).unwrap();
        let wait_b: [u8; 16] = registered_b
            .record
            .expect("principal b registered")
            .wait_id
            .try_into()
            .unwrap();
        assert_ne!(
            wait_a, wait_b,
            "the same key bytes under a different principal bind to a different durable wait"
        );

        drop(client_a);
        drop(client_b);
        harness.stop();
    }

    /// The two-phase budget must hold in BOTH directions: an inter-frame
    /// pause LONGER than the per-frame 5s read budget (but inside the idle
    /// window) still serves the next round — the idle budget, not the frame
    /// budget, governs inter-frame waits.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn inter_frame_pause_longer_than_the_frame_budget_is_served() {
        let (harness, _root) = start("pause", Duration::from_secs(30), None, false, true);
        let client = plain_client(&harness.plain_socket).await;
        client
            .exchange_validated(unique_health_request(1))
            .await
            .expect("first round");
        // 5.5s: past TransportConfig::default()'s 5s read timeout, well
        // inside the 30s idle window.
        tokio::time::sleep(Duration::from_millis(5_500)).await;
        client
            .exchange_validated(unique_health_request(2))
            .await
            .expect("round after a >5s inter-frame pause (idle budget governs)");
        drop(client);
        harness.stop();
    }

    /// Zero idle timeout is rejected at assembly (fail-closed config gate),
    /// not first at connection time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zero_connection_idle_timeout_is_rejected_at_assembly() {
        let root = TempRoot::new("zero-idle");
        fs::create_dir_all(&root.0).unwrap();
        let options = DaemonOptions::new(&root.0)
            .with_plain_socket(temp_path("zero", "plain"))
            .with_connection_idle_timeout(Duration::ZERO);
        let outcome = assemble(options, tokio::runtime::Handle::current());
        assert!(
            matches!(
                outcome,
                Err(nlos_system_control::daemon::DaemonError::Ipc(
                    nlos_ipc::IpcError::InvalidConfig(_)
                ))
            ),
            "zero idle timeout fails at assemble"
        );
    }

    /// The per-endpoint session budget closes newcomers fail-closed above
    /// the cap and frees the slot when a resident session ends.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn session_budget_closes_newcomers_above_the_cap_and_frees_on_release() {
        const BUDGET: usize = 64;
        let (harness, _root) = start("cap", Duration::from_secs(30), None, false, true);

        // Hold the full budget as resident silent connections (each parks in
        // the idle window holding one permit).
        let mut holders = Vec::new();
        for index in 0..BUDGET {
            // Parallel-suite load can surface transient connect noise on
            // macOS; each holder retries briefly (the budget is not yet
            // full, so every retry must eventually be admitted).
            let mut client = None;
            for attempt in 0..10 {
                if let Ok(admitted) = try_plain_client(&harness.plain_socket).await {
                    client = Some(admitted);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
                let _ = attempt;
            }
            holders.push(client.unwrap_or_else(|| {
                panic!("holder {index} never admitted despite budget headroom")
            }));
        }
        // The 65th connects at the kernel layer, then is closed fail-closed;
        // the rejection may surface at connect or at the first exchange.
        let started = Instant::now();
        let rejection = if let Ok(newcomer) = try_plain_client(&harness.plain_socket).await {
            newcomer
                .exchange_validated(unique_health_request(1))
                .await
                .map(|_| ())
        } else {
            Err(nlos_ipc::IpcError::ConnectionUnusable)
        };
        assert!(
            rejection.is_err(),
            "newcomer above the budget is closed: {rejection:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "rejection is prompt, not a frame-budget timeout"
        );

        // Releasing one holder ends its server session and frees the permit:
        // a fresh connection serves again (bounded retry for task teardown).
        holders.pop();
        let mut served = false;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let Ok(fresh) = try_plain_client(&harness.plain_socket).await else {
                // The freed permit races the session teardown; a rejected
                // retry just tries again.
                continue;
            };
            if fresh
                .exchange_validated(unique_health_request(2))
                .await
                .is_ok()
            {
                served = true;
                break;
            }
        }
        assert!(served, "a released slot admits a fresh resident session");
        drop(holders);
        harness.stop();
    }

    /// W82-B D3 end-to-end over the real daemon plain endpoint: an
    /// over-limit frame is answered with the typed per-call failure and the
    /// SAME connection keeps serving the next normal exchange.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn daemon_endpoint_rejects_over_limit_frame_and_keeps_serving() {
        use nlos_schema::{MAX_ENVELOPE_BYTES, decode_exchange_response};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (harness, _root) = start("d3", Duration::from_secs(30), None, false, true);
        let config = TransportConfig::default();
        let (mut stream, _) = connect(&harness.plain_socket, config)
            .await
            .expect("plain connect");

        // One over-limit frame (declared 1 MiB + 1 over the transport
        // maximum) with its full body.
        let declared = MAX_ENVELOPE_BYTES + 1;
        stream
            .write_all(&u32::try_from(declared).unwrap().to_be_bytes())
            .await
            .unwrap();
        let junk = vec![0xA5_u8; 8_192];
        for _ in 0..(declared / junk.len()) {
            stream.write_all(&junk).await.unwrap();
        }
        stream
            .write_all(&junk[..declared % junk.len()])
            .await
            .unwrap();
        stream.flush().await.unwrap();

        // The rejection is a decodable typed failure envelope.
        let mut prefix = [0_u8; 4];
        stream.read_exact(&mut prefix).await.unwrap();
        let mut rejection = vec![0_u8; u32::from_be_bytes(prefix) as usize];
        stream.read_exact(&mut rejection).await.unwrap();
        let response = decode_exchange_response(&rejection).unwrap();
        let envelope::CommonContext::ResponseContext(context) =
            response.envelope().common_context.as_ref().unwrap()
        else {
            panic!("rejection carries a response context");
        };
        let failure = context.failure.as_ref().expect("typed failure");
        assert_eq!(failure.code, i32::from(SabiErrorCode::InvalidArgument));
        assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));

        // The SAME connection serves a normal system_control exchange next.
        let health = build_request_envelope(&ControlCommand::InspectHealth).unwrap();
        let wire = nlos_schema::encode_exchange_request(&nlos_schema::sabi::v1::ExchangeRequest {
            envelope: Some(health),
        })
        .unwrap();
        stream
            .write_all(&u32::try_from(wire.len()).unwrap().to_be_bytes())
            .await
            .unwrap();
        stream.write_all(&wire).await.unwrap();
        stream.flush().await.unwrap();
        let mut prefix = [0_u8; 4];
        stream.read_exact(&mut prefix).await.unwrap();
        let mut served = vec![0_u8; u32::from_be_bytes(prefix) as usize];
        stream.read_exact(&mut served).await.unwrap();
        let response = decode_exchange_response(&served).unwrap();
        let envelope::CommonContext::ResponseContext(context) =
            response.envelope().common_context.as_ref().unwrap()
        else {
            panic!("normal round response context");
        };
        assert!(
            context.failure.is_none(),
            "normal round after rejection succeeds: {:?}",
            context.failure
        );
        drop(stream);
        harness.stop();
    }
}
