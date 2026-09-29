//! Optional [`ApplicationInspector`] adapter backed by
//! [`nlos_application::ApplicationAuthority`].
//!
//! Enabled with the crate's `application` feature; the default control prefix
//! uses [`crate::control::UnwiredApplicationInspector`] until a host wires
//! this adapter.

use nlos_application::{ApplicationAuthority, ApplicationAuthorityError, ApplicationStatus};
use nlos_schema::sabi::v1::{RetryDirective, SabiErrorCode, SabiFailure};
use nlos_types::PackageId;

use crate::control::{ApplicationInspection, ApplicationInspector};

/// Reads bounded application-head facts through the durable Application
/// authority.
pub struct ApplicationAuthorityInspector<'a> {
    authority: &'a ApplicationAuthority,
}

impl<'a> ApplicationAuthorityInspector<'a> {
    #[must_use]
    pub const fn new(authority: &'a ApplicationAuthority) -> Self {
        Self { authority }
    }
}

impl ApplicationInspector for ApplicationAuthorityInspector<'_> {
    fn inspect_application(
        &self,
        package_id: [u8; 16],
    ) -> Result<ApplicationInspection, SabiFailure> {
        let package_id = PackageId::from_bytes(package_id);
        let view = self
            .authority
            .inspect_application(package_id)
            .map_err(|error| map_application_authority_error(&error))?
            .ok_or_else(|| {
                not_found("requested application was not found under the package identity")
            })?;
        Ok(ApplicationInspection {
            package_id: *view.package_id.as_bytes(),
            application_id: *view.application_id.as_bytes(),
            package_manifest_digest: *view.package_manifest_digest.as_bytes(),
            current_installation_generation: view.current_installation_generation.get(),
            status: encode_status(view.status),
            created_at_ms: view.created_at_ms,
            updated_at_ms: view.updated_at_ms,
        })
    }
}

const fn encode_status(status: ApplicationStatus) -> u8 {
    match status {
        ApplicationStatus::Installed => 1,
        ApplicationStatus::Disabled => 2,
        ApplicationStatus::Uninstalled => 3,
    }
}

fn not_found(message: &'static str) -> SabiFailure {
    SabiFailure {
        code: SabiErrorCode::NotFound.into(),
        retry: RetryDirective::DoNotRetry.into(),
        safe_message: message.to_owned(),
    }
}

fn map_application_authority_error(error: &ApplicationAuthorityError) -> SabiFailure {
    let (code, safe_message) = match error {
        ApplicationAuthorityError::ApplicationNotFound { .. } => (
            SabiErrorCode::NotFound,
            "requested application was not found under the package identity",
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
            "application authority rejected the inspection",
        ),
    };
    SabiFailure {
        code: code.into(),
        retry: RetryDirective::DoNotRetry.into(),
        safe_message: safe_message.to_owned(),
    }
}
