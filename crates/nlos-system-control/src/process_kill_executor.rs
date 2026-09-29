//! Kill-arm [`OperationCommandExecutor`] backed by the durable
//! [`nlos_process::ProcessAuthority`] platform-kill path and the
//! [`nlos_process::SupervisorPidRegistry`] (W29-D plan row:
//! kill→SupervisorPidRegistry/platform kill).
//!
//! Execution path of one `kill_operation`:
//!
//! 1. [`ProcessAuthority::inspect_active_process_binding`] — the
//!    authoritative head record (generation + fencing token) of the
//!    targeted Process;
//! 2. the generation CAS — the wire's `expected_generation_or_revision`
//!    must equal the head generation, otherwise a typed `CONFLICT`;
//! 3. [`SupervisorPidRegistry::lookup`] — the supervisor-observed OS pid
//!    mapping, cross-checked against the head generation (`STATE` on a
//!    stale mapping; hosts feed [`SupervisorPidRegistry::pid_map`] into
//!    the platform adapter they inject here);
//! 4. [`ProcessAuthority::request_platform_kill`] — the durable kill
//!    receipt is committed first, then the injected platform adapter
//!    signals the OS (at-least-once semantics; replay re-derives the same
//!    control-plane receipt id);
//! 5. the control-plane receipt id is derived (domain-separated SHA-256)
//!    from the durable receipt bytes, so it cannot exist without the
//!    authority call.
//!
//! Every other arm refuses fail-closed: this executor owns only the kill
//! transition. Hosts compose several authorities by delegating the arms
//! they wired and refusing the rest, exactly like this type does.

use nlos_process::{
    PlatformKillAdapter, ProcessAuthority, ProcessAuthorityError, RequestPlatformKillRequest,
    SupervisorPidRegistry,
};
use nlos_schema::sabi::v1::{RetryDirective, SabiErrorCode, SabiFailure};
use nlos_types::{IdempotencyKey, ProcessId, ReceiptId};

use crate::executor_receipt::derive_executor_receipt_id;
use crate::{OperationCommandExecutor, OperationControlRequest};

const KILL_RECEIPT_DOMAIN: &[u8] = b"nlos/system-control/kill-receipt/v1";

/// Kills one operational target through the real process authority and
/// supervisor pid registry. The adapter must be `Sync`: executors are held
/// across async IPC service loops.
pub struct ProcessAuthorityKillExecutor<'a, A: PlatformKillAdapter + Sync> {
    authority: &'a ProcessAuthority,
    supervisor: &'a SupervisorPidRegistry,
    adapter: &'a A,
}

impl<'a, A: PlatformKillAdapter + Sync> ProcessAuthorityKillExecutor<'a, A> {
    /// Wires the executor. Unix hosts typically pass
    /// `&PosixPlatformKillAdapter::new(registry.pid_map())`; contract tests
    /// pass the recording [`nlos_process::StubPlatformKillAdapter`].
    #[must_use]
    pub const fn new(
        authority: &'a ProcessAuthority,
        supervisor: &'a SupervisorPidRegistry,
        adapter: &'a A,
    ) -> Self {
        Self {
            authority,
            supervisor,
            adapter,
        }
    }
}

impl<A: PlatformKillAdapter + Sync> OperationCommandExecutor
    for ProcessAuthorityKillExecutor<'_, A>
{
    fn kill_operation(&self, request: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        let process_id = ProcessId::from_bytes(request.target_id);
        let binding = self
            .authority
            .inspect_active_process_binding(process_id)
            .map_err(|error| map_process_kill_error(&error))?;
        if binding.process_generation.get() != request.expected_generation_or_revision {
            return Err(bounded_failure(
                SabiErrorCode::Conflict,
                "kill CAS mismatch: expected revision is not the process head generation",
            ));
        }
        let supervisor = self.supervisor.lookup(process_id).map_err(|_| {
            bounded_failure(
                SabiErrorCode::NotFound,
                "no supervisor os pid mapping is registered for the process",
            )
        })?;
        if supervisor.process_generation != binding.process_generation {
            return Err(bounded_failure(
                SabiErrorCode::State,
                "supervisor os pid mapping is stale for the process head generation",
            ));
        }
        let killed_at_ms = u64::try_from(request.requested_at_ms).map_err(|_| {
            bounded_failure(
                SabiErrorCode::InvalidArgument,
                "kill wall-clock reading must be non-negative",
            )
        })?;
        let decision = self
            .authority
            .request_platform_kill(
                RequestPlatformKillRequest {
                    process_id,
                    expected_process_generation: binding.process_generation,
                    expected_process_fencing_token: binding.process_fencing_token,
                    idempotency_key: IdempotencyKey::from_bytes(request.idempotency_key),
                    killed_at_ms,
                },
                self.adapter,
            )
            .map_err(|error| map_process_kill_error(&error))?;
        let receipt = decision.receipt();
        let generation = receipt.process_generation.get().to_be_bytes();
        let killed_at = receipt.killed_at_ms.to_be_bytes();
        Ok(derive_executor_receipt_id(
            KILL_RECEIPT_DOMAIN,
            &[
                receipt.process_id.as_bytes(),
                &generation,
                receipt.process_fencing_token.as_slice(),
                receipt.idempotency_key.as_bytes(),
                &killed_at,
            ],
        ))
    }

    fn pause_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Err(arm_not_wired("pause"))
    }

    fn resume_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Err(arm_not_wired("resume"))
    }

    fn cancel_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Err(arm_not_wired("cancel"))
    }

    fn throttle_operation(
        &self,
        _: OperationControlRequest,
        _: u64,
    ) -> Result<ReceiptId, SabiFailure> {
        Err(arm_not_wired("throttle"))
    }

    fn reclaim_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Err(arm_not_wired("reclaim"))
    }
}

fn arm_not_wired(arm: &'static str) -> SabiFailure {
    SabiFailure {
        code: SabiErrorCode::NotFound.into(),
        retry: RetryDirective::DoNotRetry.into(),
        safe_message: format!("the {arm} arm is not wired in the process kill executor"),
    }
}

fn bounded_failure(code: SabiErrorCode, message: &'static str) -> SabiFailure {
    SabiFailure {
        code: code.into(),
        retry: RetryDirective::DoNotRetry.into(),
        safe_message: message.to_owned(),
    }
}

fn map_process_kill_error(error: &ProcessAuthorityError) -> SabiFailure {
    let (code, safe_message) = match error {
        ProcessAuthorityError::ProcessNotFound(_) => (
            SabiErrorCode::NotFound,
            "requested process binding was not found",
        ),
        ProcessAuthorityError::ProcessBindingTerminal(_)
        | ProcessAuthorityError::StaleProcessBinding
        | ProcessAuthorityError::StaleIsolationDomain
        | ProcessAuthorityError::IsolationDomainNotFound(_) => {
            (SabiErrorCode::State, "process binding is not active")
        }
        ProcessAuthorityError::PlatformKillAlreadySignaled => (
            SabiErrorCode::Conflict,
            "platform kill was already signaled for this process generation",
        ),
        ProcessAuthorityError::PlatformKillAdapter(_) => (
            SabiErrorCode::Driver,
            "platform kill adapter failed to signal the os process",
        ),
        ProcessAuthorityError::IdempotencyConflict
        | ProcessAuthorityError::IsolationDomainFenceConflict
        | ProcessAuthorityError::ProcessFenceConflict => {
            (SabiErrorCode::Conflict, "process authority state conflict")
        }
        ProcessAuthorityError::Sqlite(_)
        | ProcessAuthorityError::DurabilityUnavailable { .. }
        | ProcessAuthorityError::CorruptRecord(_)
        | ProcessAuthorityError::SchemaVersionUnsupported(_)
        | ProcessAuthorityError::LockPoisoned => (
            SabiErrorCode::Durability,
            "process authority storage failure",
        ),
        ProcessAuthorityError::Io(_) => (SabiErrorCode::Driver, "process authority I/O failure"),
        _ => (SabiErrorCode::Driver, "process authority rejected the kill"),
    };
    bounded_failure(code, safe_message)
}
