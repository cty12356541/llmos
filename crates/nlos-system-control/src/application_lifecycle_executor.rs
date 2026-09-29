//! Application-lifecycle [`ApplicationCommandExecutor`] backed by the real
//! [`nlos_application::ApplicationAuthority`] transitions (W35-P11 plan row:
//! disable→`disable_application`; uninstall→W27-D task-activity gate +
//! `uninstall_application_with_task_activity_gate`).
//!
//! Execution path of one application arm:
//!
//! 1. [`ApplicationAuthority::inspect_application`] — the authoritative
//!    current-state view (status + current installation generation) of the
//!    targeted package identity;
//! 2. the generation CAS — the wire's `expected_generation_or_revision`
//!    must equal the application's current installation generation,
//!    otherwise a typed `CONFLICT`;
//! 3. the authority transition inside its own `Immediate` transaction —
//!    disable drives the reversible `installed → disabled` mark with no
//!    activity gate; uninstall drives the terminal
//!    `installed|disabled → uninstalled` mark **through the W27-D
//!    production activity gate**, which resolves the package's durable
//!    background-task registrations inside the gate's own transaction and
//!    queries the `SqliteTaskAuthority` for live task activity, failing
//!    closed (typed refusal, zero durable state) when the query cannot be
//!    answered;
//! 4. the control-plane receipt id is derived (domain-separated SHA-256)
//!    from the authority's own durable receipt facts, so it cannot exist
//!    without the authority call, and a durable replay re-derives the same
//!    id.
//!
//! The CAS pre-check is the control plane's declared semantic only; the
//! authority remains the final word on status inside its transaction (a
//! status refusal surfaces as its typed `STATE` failure).
use nlos_application::{
    ApplicationAuthority, ApplicationAuthorityError, DisableApplicationRequest,
    UninstallApplicationRequest,
};
use nlos_schema::sabi::v1::{RetryDirective, SabiErrorCode, SabiFailure};
use nlos_task::SqliteTaskAuthority;
use nlos_types::{IdempotencyKey, PackageId, ReceiptId};

use crate::executor_receipt::derive_executor_receipt_id;
use crate::{ApplicationCommandExecutor, ApplicationControlRequest};

const APPLICATION_DISABLE_RECEIPT_DOMAIN: &[u8] =
    b"nlos/system-control/application-disable-receipt/v1";
const APPLICATION_UNINSTALL_RECEIPT_DOMAIN: &[u8] =
    b"nlos/system-control/application-uninstall-receipt/v1";

/// Drives the two application-lifecycle arms through the real application
/// authority. Uninstall resolves the W27-D activity gate against the same
/// `SqliteTaskAuthority` the `SystemControl` handler already owns; hosts
/// pass that authority here when wiring the executor.
pub struct ApplicationAuthorityLifecycleExecutor<'a> {
    applications: &'a ApplicationAuthority,
    tasks: &'a SqliteTaskAuthority,
}

impl<'a> ApplicationAuthorityLifecycleExecutor<'a> {
    #[must_use]
    pub const fn new(
        applications: &'a ApplicationAuthority,
        tasks: &'a SqliteTaskAuthority,
    ) -> Self {
        Self {
            applications,
            tasks,
        }
    }

    fn current_generation(&self, package_id: PackageId) -> Result<u64, SabiFailure> {
        let application = self
            .applications
            .inspect_application(package_id)
            .map_err(|error| map_application_error(&error))?
            .ok_or_else(|| {
                bounded_failure(
                    SabiErrorCode::NotFound,
                    "requested application was not found under the package identity",
                )
            })?;
        Ok(application.current_installation_generation.get())
    }
}

fn cas_mismatch_failure(arm: &'static str) -> SabiFailure {
    bounded_failure(
        SabiErrorCode::Conflict,
        match arm {
            "disable" => {
                "disable CAS mismatch: expected revision is not the application installation \
                 generation"
            }
            _ => {
                "uninstall CAS mismatch: expected revision is not the application installation \
                 generation"
            }
        },
    )
}

fn wall_ms(requested_at_ms: i64, arm: &'static str) -> Result<u64, SabiFailure> {
    u64::try_from(requested_at_ms).map_err(|_| {
        bounded_failure(
            SabiErrorCode::InvalidArgument,
            match arm {
                "disable" => "disable wall-clock reading must be non-negative",
                _ => "uninstall wall-clock reading must be non-negative",
            },
        )
    })
}

impl ApplicationCommandExecutor for ApplicationAuthorityLifecycleExecutor<'_> {
    fn disable_application(
        &self,
        request: ApplicationControlRequest,
    ) -> Result<ReceiptId, SabiFailure> {
        let package_id = PackageId::from_bytes(request.package_id);
        if request.expected_generation_or_revision != self.current_generation(package_id)? {
            return Err(cas_mismatch_failure("disable"));
        }
        let disabled_at_ms = wall_ms(request.requested_at_ms, "disable")?;
        let decision = self
            .applications
            .disable_application(DisableApplicationRequest {
                package_id,
                idempotency_key: IdempotencyKey::from_bytes(request.idempotency_key),
                disabled_at_ms,
            })
            .map_err(|error| map_application_error(&error))?;
        let receipt = decision.receipt();
        let generation = receipt.application_generation.get().to_be_bytes();
        let disabled_at = receipt.disabled_at_ms.to_be_bytes();
        Ok(derive_executor_receipt_id(
            APPLICATION_DISABLE_RECEIPT_DOMAIN,
            &[
                receipt.application_id.as_bytes(),
                &generation,
                receipt.idempotency_key.as_bytes(),
                &disabled_at,
            ],
        ))
    }

    fn uninstall_application(
        &self,
        request: ApplicationControlRequest,
    ) -> Result<ReceiptId, SabiFailure> {
        let package_id = PackageId::from_bytes(request.package_id);
        if request.expected_generation_or_revision != self.current_generation(package_id)? {
            return Err(cas_mismatch_failure("uninstall"));
        }
        let uninstalled_at_ms = wall_ms(request.requested_at_ms, "uninstall")?;
        // The W27-D production activity gate: the package's durable
        // background-task registrations are resolved inside the gate's own
        // transaction and the task authority is queried for live activity
        // before the terminal transition commits.
        let decision = self
            .applications
            .uninstall_application_with_task_activity_gate(
                self.tasks,
                UninstallApplicationRequest {
                    package_id,
                    idempotency_key: IdempotencyKey::from_bytes(request.idempotency_key),
                    uninstalled_at_ms,
                },
            )
            .map_err(|error| map_application_error(&error))?;
        let receipt = decision.receipt();
        let generation = receipt.application_generation.get().to_be_bytes();
        let uninstalled_at = receipt.uninstalled_at_ms.to_be_bytes();
        Ok(derive_executor_receipt_id(
            APPLICATION_UNINSTALL_RECEIPT_DOMAIN,
            &[
                receipt.application_id.as_bytes(),
                &generation,
                receipt.idempotency_key.as_bytes(),
                &uninstalled_at,
            ],
        ))
    }
}

fn bounded_failure(code: SabiErrorCode, message: &'static str) -> SabiFailure {
    SabiFailure {
        code: code.into(),
        retry: RetryDirective::DoNotRetry.into(),
        safe_message: message.to_owned(),
    }
}

fn map_application_error(error: &ApplicationAuthorityError) -> SabiFailure {
    let (code, safe_message) = match error {
        ApplicationAuthorityError::ApplicationNotFound { .. } => (
            SabiErrorCode::NotFound,
            "requested application was not found under the package identity",
        ),
        ApplicationAuthorityError::ApplicationAlreadyDisabled { .. }
        | ApplicationAuthorityError::ApplicationAlreadyUninstalled { .. }
        | ApplicationAuthorityError::ApplicationDisabled { .. }
        | ApplicationAuthorityError::ApplicationUninstalled { .. } => (
            SabiErrorCode::State,
            "application lifecycle state rejects this command",
        ),
        ApplicationAuthorityError::ApplicationActiveTasksRunning { .. } => (
            SabiErrorCode::State,
            "application still has outstanding task activity",
        ),
        ApplicationAuthorityError::TaskActivityQueryFailed { .. } => (
            SabiErrorCode::Driver,
            "task activity query failed; uninstall refused fail-closed",
        ),
        ApplicationAuthorityError::DisablePrecedesInstallation { .. }
        | ApplicationAuthorityError::UninstallPrecedesLastUpdate { .. } => (
            SabiErrorCode::InvalidArgument,
            "command timestamp precedes the application's last durable update",
        ),
        ApplicationAuthorityError::IdempotencyConflict => (
            SabiErrorCode::Conflict,
            "application authority idempotency conflict",
        ),
        ApplicationAuthorityError::Sqlite(_)
        | ApplicationAuthorityError::Io(_)
        | ApplicationAuthorityError::DurabilityUnavailable { .. }
        | ApplicationAuthorityError::LockPoisoned => (
            SabiErrorCode::Durability,
            "application authority storage failure",
        ),
        ApplicationAuthorityError::SchemaVersionUnsupported(_)
        | ApplicationAuthorityError::CorruptRecord(_) => (
            SabiErrorCode::Driver,
            "local application authority state is invalid",
        ),
        _ => (
            SabiErrorCode::Driver,
            "application authority rejected the command",
        ),
    };
    bounded_failure(code, safe_message)
}
