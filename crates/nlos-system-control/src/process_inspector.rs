//! Optional [`ProcessInspector`] adapter backed by [`nlos_process::ProcessAuthority`].
//!
//! Enabled with the crate's `process` feature; the default control prefix uses
//! [`crate::control::UnwiredProcessInspector`] until a host wires this adapter.

use nlos_process::{ProcessAuthority, ProcessAuthorityError};
use nlos_schema::sabi::v1::{RetryDirective, SabiErrorCode, SabiFailure};
use nlos_types::ProcessId;

use crate::control::{ProcessInspection, ProcessInspector};

/// Reads bounded process-binding facts through the durable Process authority.
pub struct ProcessAuthorityInspector<'a> {
    authority: &'a ProcessAuthority,
}

impl<'a> ProcessAuthorityInspector<'a> {
    #[must_use]
    pub const fn new(authority: &'a ProcessAuthority) -> Self {
        Self { authority }
    }
}

impl ProcessInspector for ProcessAuthorityInspector<'_> {
    fn inspect_process(&self, process_id: [u8; 16]) -> Result<ProcessInspection, SabiFailure> {
        let process_id = ProcessId::from_bytes(process_id);
        let record = self
            .authority
            .inspect_active_process_binding(process_id)
            .map_err(map_process_authority_error)?;
        Ok(ProcessInspection {
            process_id: *record.process_id.as_bytes(),
            process_generation: record.process_generation.get(),
            agent_instance_id: *record.agent_instance_id.as_bytes(),
            task_id: *record.task_id.as_bytes(),
            task_attempt_id: *record.task_attempt_id.as_bytes(),
            isolation_domain_id: *record.isolation_domain_id.as_bytes(),
        })
    }
}

fn map_process_authority_error(error: ProcessAuthorityError) -> SabiFailure {
    let (code, retry, safe_message) = match error {
        ProcessAuthorityError::ProcessNotFound(_) => (
            SabiErrorCode::NotFound,
            RetryDirective::DoNotRetry,
            "requested process binding was not found",
        ),
        ProcessAuthorityError::ProcessBindingTerminal(_)
        | ProcessAuthorityError::StaleProcessBinding
        | ProcessAuthorityError::StaleIsolationDomain
        | ProcessAuthorityError::StaleFiberIncarnation
        | ProcessAuthorityError::IsolationDomainNotFound(_) => (
            SabiErrorCode::State,
            RetryDirective::DoNotRetry,
            "process binding is not active",
        ),
        ProcessAuthorityError::Sqlite(_)
        | ProcessAuthorityError::DurabilityUnavailable { .. }
        | ProcessAuthorityError::CorruptRecord(_)
        | ProcessAuthorityError::SchemaVersionUnsupported(_)
        | ProcessAuthorityError::LockPoisoned => (
            SabiErrorCode::Durability,
            RetryDirective::DoNotRetry,
            "process authority storage failure",
        ),
        ProcessAuthorityError::Io(_) => (
            SabiErrorCode::Driver,
            RetryDirective::DoNotRetry,
            "process authority I/O failure",
        ),
        ProcessAuthorityError::IdempotencyConflict
        | ProcessAuthorityError::IsolationDomainFenceConflict
        | ProcessAuthorityError::ProcessFenceConflict => (
            SabiErrorCode::Conflict,
            RetryDirective::DoNotRetry,
            "process authority state conflict",
        ),
        ProcessAuthorityError::InvalidFiberBinding
        | ProcessAuthorityError::InvalidFiberSnapshot(_)
        | ProcessAuthorityError::GenerationExhausted => (
            SabiErrorCode::InvalidArgument,
            RetryDirective::DoNotRetry,
            "process inspection request violates the authority contract",
        ),
        ProcessAuthorityError::FiberIncarnationNotFound
        | ProcessAuthorityError::FiberSnapshotNotFound => (
            SabiErrorCode::NotFound,
            RetryDirective::DoNotRetry,
            "requested process fiber state was not found",
        ),
        ProcessAuthorityError::FiberIncarnationCancelled(_) => (
            SabiErrorCode::State,
            RetryDirective::DoNotRetry,
            "process binding is not active",
        ),
    };
    SabiFailure {
        code: code.into(),
        retry: retry.into(),
        safe_message: safe_message.to_owned(),
    }
}
