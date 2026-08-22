//! Focused conformance for the bounded `SystemControl` failure surface.
//!
//! These tests intentionally exercise the transport-neutral mapping directly:
//! the handler's rejected `Result` must be convertible to a valid, sanitized
//! SABI failure without manufacturing an Operation or Receipt.

use std::path::PathBuf;

use nlos_schema::sabi::v1::{
    CallerIdentity, Envelope, RetryDirective, SabiErrorCode, SabiRequestContext, SchemaIdentity,
    envelope,
};
use nlos_schema::{
    CommonSemanticsError, CompatibilityError, REQUEST_ID_BYTES, SABI_ENVELOPE_SCHEMA,
};
use nlos_system_control::{SYSTEM_CONTROL_SERVICE, SystemControlError, failure_envelope};
use nlos_task::{ArtifactRecoveryState, TaskStoreError};

fn assert_mapping(
    error: &SystemControlError,
    expected_code: SabiErrorCode,
    expected_retry: RetryDirective,
) {
    let failure = error.to_sabi_failure();
    assert_eq!(failure.code, i32::from(expected_code));
    assert_eq!(failure.retry, i32::from(expected_retry));
    assert!(!failure.safe_message.is_empty());
    assert!(failure.safe_message.len() <= 256);
    assert!(!failure.safe_message.contains('\0'));
}

#[test]
fn request_contract_and_identity_failures_are_terminal_and_bounded() {
    assert_mapping(
        &SystemControlError::Schema(CompatibilityError::MissingSystemControlCommand),
        SabiErrorCode::InvalidArgument,
        RetryDirective::DoNotRetry,
    );
    assert_mapping(
        &SystemControlError::Common(CommonSemanticsError::MissingIdempotencyKey),
        SabiErrorCode::InvalidArgument,
        RetryDirective::DoNotRetry,
    );
    assert_mapping(
        &SystemControlError::Common(CommonSemanticsError::DeadlineExpired),
        SabiErrorCode::Deadline,
        RetryDirective::DoNotRetry,
    );
    assert_mapping(
        &SystemControlError::AuthorizationDenied("untrusted local diagnostic"),
        SabiErrorCode::Rights,
        RetryDirective::DoNotRetry,
    );
    assert_mapping(
        &SystemControlError::CallerIssuerMismatch,
        SabiErrorCode::Rights,
        RetryDirective::DoNotRetry,
    );
    assert_mapping(
        &SystemControlError::CommandIdempotencyMismatch,
        SabiErrorCode::Conflict,
        RetryDirective::DoNotRetry,
    );
    assert_mapping(
        &SystemControlError::UnknownMethod,
        SabiErrorCode::NotSupported,
        RetryDirective::DoNotRetry,
    );
    assert_mapping(
        &SystemControlError::InvalidRecoveryAlert,
        SabiErrorCode::Driver,
        RetryDirective::DoNotRetry,
    );
}

#[test]
fn recovery_task_failures_preserve_retry_safety() {
    assert_mapping(
        &SystemControlError::Task(TaskStoreError::ArtifactRecoveryNotFound),
        SabiErrorCode::NotFound,
        RetryDirective::DoNotRetry,
    );
    assert_mapping(
        &SystemControlError::Task(TaskStoreError::ArtifactRecoveryCasMismatch {
            expected: 1,
            current: 2,
        }),
        SabiErrorCode::Conflict,
        RetryDirective::DoNotRetry,
    );
    assert_mapping(
        &SystemControlError::Task(TaskStoreError::InvalidArtifactRecoveryState {
            state: ArtifactRecoveryState::Retrying,
        }),
        SabiErrorCode::State,
        RetryDirective::DoNotRetry,
    );
    assert_mapping(
        &SystemControlError::Task(TaskStoreError::InvalidArtifactRecoveryPolicy {
            reason: "invalid timestamp",
        }),
        SabiErrorCode::InvalidArgument,
        RetryDirective::DoNotRetry,
    );
    assert_mapping(
        &SystemControlError::Task(TaskStoreError::DurabilityUnavailable {
            journal_mode: "delete".to_owned(),
            synchronous: 1,
        }),
        SabiErrorCode::Durability,
        RetryDirective::DoNotRetry,
    );
    assert_mapping(
        &SystemControlError::Task(TaskStoreError::CorruptRecord("private durable detail")),
        SabiErrorCode::Driver,
        RetryDirective::DoNotRetry,
    );
}

#[test]
fn sqlite_open_failure_is_retryable_with_the_same_key() {
    let missing_parent = std::env::temp_dir().join(format!(
        "nlos-system-control-missing-parent-{}",
        std::process::id()
    ));
    let path = missing_parent.join(PathBuf::from("authority.sqlite3"));
    let Err(error) = nlos_task::SqliteTaskAuthority::open(path) else {
        panic!("opening under a missing parent unexpectedly succeeded");
    };
    assert_mapping(
        &SystemControlError::Task(error),
        SabiErrorCode::Durability,
        RetryDirective::RetrySameIdempotencyKey,
    );
}

#[test]
fn failure_envelope_retains_correlation_and_carries_no_receipts() {
    let correlation_id = vec![0x41; REQUEST_ID_BYTES];
    let request_id = vec![0x42; REQUEST_ID_BYTES];
    let request = Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: 1,
            minor: 1,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        request_id: request_id.clone(),
        service: SYSTEM_CONTROL_SERVICE.to_owned(),
        method: "submit".to_owned(),
        common_context: Some(envelope::CommonContext::RequestContext(
            SabiRequestContext {
                caller: Some(CallerIdentity {
                    principal_id: vec![0x10; REQUEST_ID_BYTES],
                    application_id: vec![0x11; REQUEST_ID_BYTES],
                    process_id: vec![0x12; REQUEST_ID_BYTES],
                    process_generation: 1,
                }),
                activity_context: Vec::new(),
                task_execution_binding: None,
                correlation_id: correlation_id.clone(),
                idempotency_key: vec![0x13; REQUEST_ID_BYTES],
                deadline_monotonic_ns: 0,
                capability_handles: Vec::new(),
                reservation_handle: None,
                proposal_or_input_digest_sha256: Vec::new(),
            },
        )),
        payload: vec![0xaa, 0xbb],
    };

    let response = failure_envelope(
        &request,
        &SystemControlError::AuthorizationDenied("must not cross boundary"),
    );
    assert_eq!(response.request_id, request_id);
    assert_eq!(response.service, SYSTEM_CONTROL_SERVICE);
    assert_eq!(response.method, "submit");
    assert!(response.payload.is_empty());
    let envelope::CommonContext::ResponseContext(context) =
        response.common_context.expect("failure response context")
    else {
        panic!("failure envelope must carry a response context");
    };
    assert_eq!(context.correlation_id, correlation_id);
    assert!(context.operation.is_none());
    assert!(context.receipts.is_empty());
    let failure = context.failure.expect("typed failure");
    assert_eq!(failure.code, i32::from(SabiErrorCode::Rights));
    assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));
    assert_eq!(failure.safe_message, "SystemControl authorization denied");
}

#[test]
fn malformed_correlation_falls_back_to_a_bounded_request_id() {
    let request = Envelope {
        request_id: vec![0x52; REQUEST_ID_BYTES],
        common_context: Some(envelope::CommonContext::RequestContext(
            SabiRequestContext {
                correlation_id: vec![0x53],
                ..SabiRequestContext::default()
            },
        )),
        ..Envelope::default()
    };
    let response = failure_envelope(&request, &SystemControlError::UnknownMethod);
    let envelope::CommonContext::ResponseContext(context) =
        response.common_context.expect("failure response context")
    else {
        panic!("failure envelope must carry a response context");
    };
    assert_eq!(context.correlation_id, request.request_id);
}
