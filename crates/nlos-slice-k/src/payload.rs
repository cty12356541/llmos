//! The kernel-side payload-execution lane (handover #2 first slice,
//! W35-P2 / `C-APP-PAYLOAD` 前片): for one installed application's
//! manifest-declared `executable` entry, really execute the payload bytes
//! through the public `nlos-driver-mock` provider face.
//!
//! Interpretation (grounded, not invented): the sample app's manifest
//! declares `entry = <name> executable <path>` — an executable payload is
//! a named byte entry whose artifact identity is the deterministic
//! `derive_artifact_id(package_id, version, name)` and whose bytes were
//! materialized into the artifact authority at verify time. W30-B's
//! driver contract defines what "executing" means on the provider face:
//! one operation driven `register → dispatch → complete`, where
//! `complete` derives its terminal outcome deterministically from
//! `(operation, callback, seed)` and every boundary leaves a real durable
//! receipt. This lane therefore:
//!
//! * resolves the application's *current* installation generation and
//!   reads the entry's materialized bytes back from the artifact
//!   authority (`get_revision`, retention-gated);
//! * derives the completion seed as a domain-separated SHA-256 over the
//!   payload bytes themselves — the bytes are consumed by the driver
//!   operation (they select the terminal outcome class and receipt), not
//!   merely registered;
//! * drives the operation through [`MockProvider`] bound to the same
//!   durable `SqliteOperationStore` the runtime's fiber lane uses, so
//!   admission/preparation/activation/terminal receipts are real and
//!   inspectable.
//!
//! No runtime beyond the existing driver face is invented: no process
//! spawn, no script interpreter, no wasm engine. Determinism is total for
//! a fixed durable state and payload bytes; an exact re-execution replays
//! every receipt; payload bytes mutated under an already-terminal
//! callback identity are typed-rejected
//! (`OperationError::CallbackIdentityConflict` via
//! [`SliceKError::Driver`]), never silently re-derived; a new
//! installation generation derives a fresh operation identity and
//! executes the new generation's bytes.

use std::sync::Arc;

use nlos_application::ApplicationStatus;
use nlos_artifact::{ContentDigest, derive_artifact_id};
use nlos_driver_mock::{CompleteProviderOperation, DispatchProviderOperation, MockProvider};
use nlos_operation::{CompletionOutcome, OperationHandle, OperationState};
use nlos_runtime::FiberHandle;
use nlos_types::{
    ApplicationId, ArtifactId, CallbackId, CancellationScopeId, ExecutionFiberId, Generation,
    OperationId, PackageId, ReceiptId,
};
use sha2::{Digest, Sha256};

use crate::error::{SliceKError, SliceKResult};
use crate::runtime::SliceKRuntime;

/// Domain separator of the payload operation identity derivation: one
/// SHA-256 over `domain ‖ application_id ‖ generation ‖ entry_name ‖ tag`
/// yields the operation id, the synthetic owner-fiber id, the callback
/// id, and the cancellation-scope id (tags 0–3). Domain-separated from
/// every authority derivation and from the sample driver's key domains.
pub const PAYLOAD_OPERATION_DOMAIN: &[u8] = b"llmos/slice-k/payload-operation/v1";

/// Domain separator of the completion seed: SHA-256 over
/// `domain ‖ payload_bytes`. This is the point where the executable
/// bytes enter the driver operation: the provider's terminal outcome is
/// a pure function of `(operation, callback, seed)`, hence of the bytes.
pub const PAYLOAD_SEED_DOMAIN: &[u8] = b"llmos/slice-k/payload-seed/v1";

/// The receipts of one payload execution, straight from the driver face.
#[derive(Clone, Debug)]
pub struct PayloadExecution {
    pub application_id: ApplicationId,
    pub package_id: PackageId,
    pub entry_name: String,
    /// The installation generation whose package version resolved the
    /// entry artifact (updates derive a fresh operation identity).
    pub installation_generation: Generation,
    pub package_version: u64,
    pub artifact_id: ArtifactId,
    /// The artifact revision whose bytes were consumed.
    pub payload_revision: u64,
    pub payload_digest: ContentDigest,
    pub payload_size_bytes: u64,
    pub operation: OperationHandle,
    pub callback_id: CallbackId,
    /// Authority-derived endpoint admission receipt (`register`).
    pub admission_receipt_id: ReceiptId,
    /// Durable prepare receipt (`dispatch`).
    pub preparation_receipt_id: ReceiptId,
    /// Durable activate receipt (`dispatch`).
    pub activation_receipt_id: ReceiptId,
    /// The seed-derived terminal outcome of the executed payload.
    pub outcome: CompletionOutcome,
    pub terminal_state: OperationState,
    pub register_replayed: bool,
    pub dispatch_replayed: bool,
    pub complete_replayed: bool,
}

impl PayloadExecution {
    /// Stable `key=value` lines for inspect/demo output (grep-friendly).
    #[must_use]
    pub fn report_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(format!(
            "application={} generation={} status=executed",
            crate::short_hex(self.application_id.as_bytes()),
            self.installation_generation.get()
        ));
        lines.push(format!(
            "entry={} artifact={} revision={} digest={} bytes={}",
            self.entry_name,
            crate::short_hex(self.artifact_id.as_bytes()),
            self.payload_revision,
            crate::short_hex(self.payload_digest.as_bytes()),
            self.payload_size_bytes
        ));
        lines.push(format!(
            "operation={} callback={}",
            crate::short_hex(self.operation.operation_id.as_bytes()),
            crate::short_hex(self.callback_id.as_bytes())
        ));
        lines.push(format!(
            "receipts admission={} preparation={} activation={}",
            crate::short_hex(self.admission_receipt_id.as_bytes()),
            crate::short_hex(self.preparation_receipt_id.as_bytes()),
            crate::short_hex(self.activation_receipt_id.as_bytes())
        ));
        lines.push(format!(
            "outcome={} terminal_state={:?} replayed={}{}{}",
            match self.outcome {
                CompletionOutcome::Completed { receipt_id } => {
                    format!("completed:{}", crate::short_hex(receipt_id.as_bytes()))
                }
                CompletionOutcome::Failed { receipt_id } => {
                    format!("failed:{}", crate::short_hex(receipt_id.as_bytes()))
                }
                CompletionOutcome::PartialEffect { receipt_id } => {
                    format!("partial-effect:{}", crate::short_hex(receipt_id.as_bytes()))
                }
                CompletionOutcome::EffectUnknown { receipt_id } => {
                    format!("effect-unknown:{}", crate::short_hex(receipt_id.as_bytes()))
                }
                CompletionOutcome::CancelledBeforeEffect { receipt_id } => {
                    format!(
                        "cancelled-before-effect:{}",
                        crate::short_hex(receipt_id.as_bytes())
                    )
                }
            },
            self.terminal_state,
            self.register_replayed,
            self.dispatch_replayed,
            self.complete_replayed
        ));
        lines
    }
}

/// The completion seed of one payload: domain-separated SHA-256 over the
/// bytes themselves. Public so consumers (and tests) can recompute the
/// expected provider outcome via
/// [`derive_provider_outcome`](nlos_driver_mock::derive_provider_outcome).
#[must_use]
pub fn payload_execution_seed(payload: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(PAYLOAD_SEED_DOMAIN);
    hasher.update(payload);
    hasher.finalize().into()
}

fn derive_identity(
    application_id: ApplicationId,
    generation: Generation,
    entry_name: &str,
    tag: u8,
) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(PAYLOAD_OPERATION_DOMAIN);
    hasher.update(application_id.as_bytes());
    hasher.update(generation.get().to_be_bytes());
    hasher.update(
        u32::try_from(entry_name.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    hasher.update(entry_name.as_bytes());
    hasher.update([tag]);
    let digest: [u8; 32] = hasher.finalize().into();
    let mut identity = [0_u8; 16];
    identity.copy_from_slice(&digest[..16]);
    identity
}

/// Executes one installed application's manifest-declared executable
/// entry through the driver face.
///
/// The entry artifact is resolved by the manifest-declared entry name via
/// the deterministic `derive_artifact_id(package_id, version, name)` over
/// the *current* installation generation's package version, and the bytes
/// are read back from the artifact authority (head revision,
/// retention-gated). The manifest's role declaration for the entry is not
/// durable post-verify (the installation receipt binds the manifest
/// digest, not the parsed manifest); the caller names an entry its signed
/// manifest declared `executable`.
///
/// # Errors
///
/// Fails typed with [`SliceKError::PayloadState`] when no application is
/// installed under the package identity, its status is not `installed`,
/// it has no installation receipt, or the named entry was never
/// materialized; with [`SliceKError::Artifact`] when the authority
/// refuses the readback; and with [`SliceKError::Driver`] when the
/// provider face refuses a boundary (including the callback-identity
/// conflict raised for payload bytes that mutated after a first
/// execution under the same identity).
pub fn execute_application_payload(
    runtime: &SliceKRuntime,
    package_id: PackageId,
    entry_name: &str,
) -> SliceKResult<PayloadExecution> {
    let application = runtime
        .applications
        .inspect_application(package_id)?
        .ok_or(SliceKError::PayloadState(
            "no application is installed under this package identity",
        ))?;
    if application.status != ApplicationStatus::Installed {
        return Err(SliceKError::PayloadState(
            "application status is not installed; execution is refused",
        ));
    }
    let installation = runtime
        .applications
        .list_installations(application.application_id)?
        .into_iter()
        .max_by_key(|receipt| receipt.installation_generation.get())
        .ok_or(SliceKError::PayloadState(
            "installed application has no installation receipt",
        ))?;
    let artifact_id = ArtifactId::from_bytes(derive_artifact_id(
        package_id,
        installation.package_version,
        entry_name,
    ));
    let head = runtime
        .artifacts
        .resolve_head(artifact_id, u64::MAX)?
        .ok_or(SliceKError::PayloadState(
            "executable entry artifact has no revision; the package was never materialized",
        ))?;
    let payload = runtime
        .artifacts
        .get_revision(artifact_id, head.revision, u64::MAX)?;
    let payload_size_bytes = u64::try_from(payload.len()).unwrap_or(u64::MAX);

    // The payload operation owns itself in the minimal slice: there is no
    // ambient task fiber here, so the owner-fiber and cancellation-scope
    // identities are derived in the same domain-separated space as the
    // operation id (the driver-mock fixture discipline: the durable
    // operation store records these identities; it does not resolve them
    // against a fiber registry).
    let operation_id = OperationId::from_bytes(derive_identity(
        application.application_id,
        installation.installation_generation,
        entry_name,
        0,
    ));
    let callback_id = CallbackId::from_bytes(derive_identity(
        application.application_id,
        installation.installation_generation,
        entry_name,
        1,
    ));
    let spec = nlos_operation::OperationSpec {
        operation_id,
        generation: Generation::INITIAL,
        owner_fiber: FiberHandle {
            fiber_id: ExecutionFiberId::from_bytes(derive_identity(
                application.application_id,
                installation.installation_generation,
                entry_name,
                2,
            )),
            generation: Generation::INITIAL,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes(derive_identity(
            application.application_id,
            installation.installation_generation,
            entry_name,
            3,
        )),
        cancellation_generation: Generation::INITIAL,
    };

    let provider = MockProvider::new(Arc::clone(&runtime.operations));
    let registered = provider.register(spec)?;
    let dispatched = provider.dispatch(DispatchProviderOperation {
        handle: registered.handle,
        callback_id,
    })?;
    let completed = provider.complete(CompleteProviderOperation {
        handle: registered.handle,
        callback_id,
        seed: payload_execution_seed(&payload),
    })?;
    let payload_digest = ContentDigest::of_bytes(&payload);
    Ok(PayloadExecution {
        application_id: application.application_id,
        package_id,
        entry_name: entry_name.to_string(),
        installation_generation: installation.installation_generation,
        package_version: installation.package_version,
        artifact_id,
        payload_revision: head.revision,
        payload_digest,
        payload_size_bytes,
        operation: registered.handle,
        callback_id,
        admission_receipt_id: registered.admission_receipt_id,
        preparation_receipt_id: dispatched.preparation_receipt_id,
        activation_receipt_id: dispatched.activation_receipt_id,
        outcome: completed.outcome,
        terminal_state: completed.state,
        register_replayed: registered.replayed,
        dispatch_replayed: dispatched.replayed,
        complete_replayed: completed.replayed,
    })
}
