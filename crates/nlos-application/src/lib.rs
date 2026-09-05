//! Minimal durable Application/Installation authority (Slice K §23.1
//! prefix, B-APPLICATION-001).
//!
//! This slice turns a *verified* signed Package into durable installation
//! state: [`ApplicationAuthority::install_application`] consumes the
//! `nlos-artifact` verified-package receipt **by receipt id only**
//! (authority-first — the caller never supplies a verification conclusion,
//! it names the fact and the artifact authority owns its content), then in
//! one `Immediate` transaction commits the immutable installation receipt
//! and CAS-advances the application's current installation generation.
//!
//! Schema v2 keeps three tables: `applications` (the current-state
//! singleton per package identity — derived [`ApplicationId`], package
//! manifest digest of the current installation, current installation
//! generation, and the §23.1 minimal lifecycle status `installed`/
//! `disabled`/`uninstalled`), `installation_receipts` (the immutable
//! installation facts — installation id, package digest, manifest digest,
//! installer principal, idempotency key, timestamps), and — added in v2,
//! mirroring the artifact authority's staged migrations —
//! `application_disable_receipts` (the immutable fact of the one
//! `installed → disabled` transition, which makes
//! [`ApplicationAuthority::disable_application`] replayable). Schema v3
//! adds `application_uninstall_receipts` (the immutable fact of the terminal
//! `installed|disabled → uninstalled` transition, which makes
//! [`ApplicationAuthority::uninstall_application`] replayable). Schema v4
//! adds `application_rollback_receipts` (the immutable fact of one
//! `disabled|uninstalled → installed` generation step back, which makes
//! [`ApplicationAuthority::rollback_application`] replayable). DDL triggers
//! carry the invariants at every layer: receipts are immutable and
//! durable, the generation is monotonic under CAS, application identity is
//! frozen, rows cannot be deleted, a receipt can only exist at the
//! application's *current* generation (receipt and generation advance
//! share one transaction, like the clock's watermark-bounded tick
//! receipts), and a disable receipt can only exist for an application
//! already disabled at its current generation.
//!
//! Verify-then-commit order, fail-closed: artifact receipt readback (the
//! FINALIZED gate — an immutable verified receipt either exists whole or
//! not at all) → idempotent replay (the durable installation receipt is
//! the authority and replays without re-consulting anything) → the
//! seven-equation digest binding between the committed installation row
//! and the artifact receipt (receipt id, package id, manifest digest,
//! package version, entry count, installer principal, and
//! `installed_at_ms >= verified_at_ms`) → single-transaction commit. A
//! replayed key returns the durably recorded original receipt without
//! moving the generation (no double-jump); every rejection leaves zero
//! durable state.
//!
//! The slice deliberately does not implement: the full §23.1
//! migrate/rollback lifecycle and any policy engine beyond the minimal
//! prefixes ([`ApplicationAuthority::update_application`] lands the
//! installed-state content update prefix only; `disable_application` lands
//! the `installed → disabled` transition only; `uninstall_application`
//! lands the terminal `installed|disabled → uninstalled` CAS mark only;
//! `rollback_application` lands one generation step back from
//! `disabled|uninstalled` to `installed` only; there is no enable shortcut,
//! no physical row delete), running Task/Process teardown (ungated
//! `uninstall_application`/`rollback_application` do not stop or wait for
//! tasks; [`ApplicationAuthority::uninstall_application_with_activity_gate`]
//! and [`ApplicationAuthority::rollback_application_with_activity_gate`]
//! refuse fresh mutations when a caller-supplied [`ActiveTaskActivityProbe`]
//! reports outstanding Tasks),
//! creation wiring (the next Slice K longitudinal slice), multi-party
//! installer approval (exactly one installer principal is recorded, taken
//! from the verified receipt's signer), §23.2's full manifest
//! applications/components model, and any cross-process transport.

mod schema;

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_artifact::{ArtifactError, ArtifactStore, ContentDigest, PackageVerificationReceipt};
use nlos_types::{
    ApplicationId, Generation, IdempotencyKey, InstallationId, PackageId, PrincipalId, ReceiptId,
};
use rusqlite::{Connection, TransactionBehavior, params};
use sha2::{Digest, Sha256};

/// Domain separator for the authority-derived [`ApplicationId`]: one
/// application identity per package identity, derived exactly like the
/// artifact receipt id (domain-tagged SHA-256, truncated to 16 bytes).
const APPLICATION_ID_DOMAIN: &[u8] = b"llmos/application/application-id/v1";

/// Domain separator for the authority-derived [`InstallationId`].
const INSTALLATION_ID_DOMAIN: &[u8] = b"llmos/application/installation-id/v1";

/// Lifecycle status of an application: the §23.1 minimal subset. `Disabled`
/// is representable and durably terminal in this slice (no policy engine);
/// only `Installed` applications accept new installations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplicationStatus {
    /// The application's current installation is active.
    Installed,
    /// The application has been disabled; new installations are refused.
    Disabled,
    /// The application has been uninstalled; the row remains durable evidence.
    Uninstalled,
}

impl ApplicationStatus {
    const fn encode(self) -> i64 {
        match self {
            Self::Installed => 1,
            Self::Disabled => 2,
            Self::Uninstalled => 3,
        }
    }

    fn decode(value: i64) -> Result<Self, ApplicationAuthorityError> {
        match value {
            1 => Ok(Self::Installed),
            2 => Ok(Self::Disabled),
            3 => Ok(Self::Uninstalled),
            _ => Err(ApplicationAuthorityError::CorruptRecord(
                "unknown application status",
            )),
        }
    }
}

/// Read-only view of an application's current durable state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplicationView {
    pub application_id: ApplicationId,
    pub package_id: PackageId,
    /// Manifest digest of the currently installed package generation.
    pub package_manifest_digest: ContentDigest,
    /// Current installation generation (dense, starts at 1, never reuses a
    /// value, never decreases).
    pub current_installation_generation: Generation,
    pub status: ApplicationStatus,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Immutable durable proof that one verified package was installed as one
/// application generation. Every field is copied bitwise from the artifact
/// authority's verified receipt (digest binding) or derived by this
/// authority; nothing is caller-supplied except the idempotency key and the
/// installation timestamp.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallationReceipt {
    pub installation_id: InstallationId,
    pub application_id: ApplicationId,
    pub installation_generation: Generation,
    pub package_id: PackageId,
    pub package_manifest_digest: ContentDigest,
    pub package_version: u64,
    pub entry_count: u64,
    /// The artifact authority's verified-package receipt this installation
    /// was derived from.
    pub package_verification_receipt_id: ReceiptId,
    /// The verified package signer (the installer principal of record).
    pub installer_principal: PrincipalId,
    pub idempotency_key: IdempotencyKey,
    pub installed_at_ms: u64,
}

/// Request to install one verified package. Authority-first: the caller
/// references the verification fact by its artifact-authority receipt id
/// and supplies no verification conclusion of its own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InstallApplicationRequest {
    /// Receipt id of the verified signed package in the artifact authority.
    pub package_verification_receipt_id: ReceiptId,
    /// Caller-supplied exactly-once key for the installation receipt.
    pub idempotency_key: IdempotencyKey,
    /// Caller-supplied installation timestamp (ms since Unix epoch); must
    /// not precede the package's verification timestamp.
    pub installed_at_ms: u64,
}

/// Outcome of one [`ApplicationAuthority::install_application`] call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InstallDecision {
    /// First execution of this key: the application generation advanced and
    /// the installation receipt committed with it.
    Installed(InstallationReceipt),
    /// Durable replay: this key already installed and the recorded original
    /// receipt is returned unchanged (no re-advance, no double-jump).
    Replayed(InstallationReceipt),
}

impl InstallDecision {
    /// The installation receipt this call denotes, whichever branch.
    #[must_use]
    pub const fn receipt(self) -> InstallationReceipt {
        match self {
            Self::Installed(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

/// Immutable durable proof that one application was disabled: the fact of
/// the one `installed → disabled` transition. The receipt is uniquely
/// addressed by the application it disabled (the status is terminal, so at
/// most one disable receipt can ever exist per application) and records
/// the installation generation at disable time, which the transition
/// leaves untouched.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisableReceipt {
    pub application_id: ApplicationId,
    /// The installation generation at disable time (unchanged by the
    /// transition — the state-machine trigger forbids moving it).
    pub application_generation: Generation,
    pub idempotency_key: IdempotencyKey,
    pub disabled_at_ms: u64,
}

/// Request to disable one installed application. Exactly-once by
/// idempotency key, mirroring the install request shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisableApplicationRequest {
    /// The package identity whose application singleton is disabled.
    pub package_id: PackageId,
    /// Caller-supplied exactly-once key for the disable receipt.
    pub idempotency_key: IdempotencyKey,
    /// Caller-supplied disable timestamp (ms since Unix epoch); must not
    /// precede the current installation's install timestamp.
    pub disabled_at_ms: u64,
}

/// Outcome of one [`ApplicationAuthority::disable_application`] call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DisableDecision {
    /// First execution of this key: the application status CAS'd
    /// `installed → disabled` and the disable receipt committed with it.
    Disabled(DisableReceipt),
    /// Durable replay: this key already disabled the application and the
    /// recorded original receipt is returned unchanged (no second
    /// transition, no new fact).
    Replayed(DisableReceipt),
}

impl DisableDecision {
    /// The disable receipt this call denotes, whichever branch.
    #[must_use]
    pub const fn receipt(self) -> DisableReceipt {
        match self {
            Self::Disabled(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

/// Request to update one installed application to a new verified package
/// generation. Authority-first: the caller references the verification
/// fact by its artifact-authority receipt id and names the package
/// identity whose application singleton is updated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UpdateApplicationRequest {
    /// The package identity whose installed application is updated.
    pub package_id: PackageId,
    /// Receipt id of the newly verified signed package in the artifact
    /// authority.
    pub package_verification_receipt_id: ReceiptId,
    /// Caller-supplied exactly-once key for this update's installation
    /// receipt.
    pub idempotency_key: IdempotencyKey,
    /// Caller-supplied update timestamp (ms since Unix epoch); must not
    /// precede the new package's verification timestamp.
    pub updated_at_ms: u64,
}

/// Outcome of one [`ApplicationAuthority::update_application`] call. The
/// committed fact is always an immutable [`InstallationReceipt`] at the
/// new installation generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateDecision {
    /// First execution of this key: the application generation advanced and
    /// the installation receipt committed with it.
    Updated(InstallationReceipt),
    /// Durable replay: this key already updated and the recorded original
    /// receipt is returned unchanged (no re-advance, no double-jump).
    Replayed(InstallationReceipt),
}

impl UpdateDecision {
    /// The installation receipt this call denotes, whichever branch.
    #[must_use]
    pub const fn receipt(self) -> InstallationReceipt {
        match self {
            Self::Updated(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

/// Immutable durable proof that one application was uninstalled: the fact of
/// the terminal `installed|disabled → uninstalled` transition. The receipt
/// is uniquely addressed by the application it uninstalled (the status is
/// terminal, so at most one uninstall receipt can ever exist per
/// application) and records the installation generation at uninstall time,
/// which the transition leaves untouched.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UninstallReceipt {
    pub application_id: ApplicationId,
    /// The installation generation at uninstall time (unchanged by the
    /// transition — the state-machine trigger forbids moving it).
    pub application_generation: Generation,
    pub idempotency_key: IdempotencyKey,
    pub uninstalled_at_ms: u64,
}

/// Request to uninstall one installed or disabled application. Exactly-once
/// by idempotency key, mirroring the disable request shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UninstallApplicationRequest {
    /// The package identity whose application singleton is uninstalled.
    pub package_id: PackageId,
    /// Caller-supplied exactly-once key for the uninstall receipt.
    pub idempotency_key: IdempotencyKey,
    /// Caller-supplied uninstall timestamp (ms since Unix epoch); must not
    /// precede the current application row's last update.
    pub uninstalled_at_ms: u64,
}

/// Outcome of one [`ApplicationAuthority::uninstall_application`] call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UninstallDecision {
    /// First execution of this key: the application status CAS'd to
    /// `uninstalled` and the uninstall receipt committed with it.
    Uninstalled(UninstallReceipt),
    /// Durable replay: this key already uninstalled the application and the
    /// recorded original receipt is returned unchanged (no second
    /// transition, no new fact).
    Replayed(UninstallReceipt),
}

impl UninstallDecision {
    /// The uninstall receipt this call denotes, whichever branch.
    #[must_use]
    pub const fn receipt(self) -> UninstallReceipt {
        match self {
            Self::Uninstalled(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

/// Immutable durable proof that one application rolled back one installation
/// generation: the fact of one `disabled|uninstalled → installed` step that
/// CAS-decrements the current generation and restores the previous
/// installation's manifest digest from durable history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RollbackReceipt {
    pub application_id: ApplicationId,
    /// The installation generation before rollback (the generation being
    /// stepped back from).
    pub from_generation: Generation,
    /// The installation generation after rollback (the previous durable
    /// installation generation restored as current).
    pub to_generation: Generation,
    pub idempotency_key: IdempotencyKey,
    pub rollback_at_ms: u64,
}

/// Request to roll one application back one installation generation.
/// Exactly-once by idempotency key, mirroring the disable/uninstall
/// request shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RollbackApplicationRequest {
    /// The package identity whose application singleton is rolled back.
    pub package_id: PackageId,
    /// Caller-supplied exactly-once key for the rollback receipt.
    pub idempotency_key: IdempotencyKey,
    /// Caller-supplied rollback timestamp (ms since Unix epoch); must not
    /// precede the application row's last update.
    pub rollback_at_ms: u64,
}

/// Outcome of one [`ApplicationAuthority::rollback_application`] call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RollbackDecision {
    /// First execution of this key: the application status CAS'd to
    /// `installed`, the generation stepped back one, and the rollback
    /// receipt committed with it.
    RolledBack(RollbackReceipt),
    /// Durable replay: this key already rolled back and the recorded
    /// original receipt is returned unchanged (no second transition, no
    /// new fact).
    Replayed(RollbackReceipt),
}

impl RollbackDecision {
    /// The rollback receipt this call denotes, whichever branch.
    #[must_use]
    pub const fn receipt(self) -> RollbackReceipt {
        match self {
            Self::RolledBack(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

/// Caller-provided probe for outstanding Task activity under one package
/// identity. The authority does not depend on `nlos-task`; integration layers
/// supply the count.
pub trait ActiveTaskActivityProbe {
    fn outstanding_task_count(&self, package_id: PackageId) -> u64;
}

/// Fail-closed typed errors of the application/installation authority.
/// Every variant is a hard refusal: the caller never receives an
/// installation whose durability is in doubt, and a rejected install
/// leaves zero durable state.
#[derive(Debug)]
pub enum ApplicationAuthorityError {
    /// Storage failure (injected I/O error, disk full, corruption surfaced
    /// by `SQLite`).
    Sqlite(rusqlite::Error),
    /// Filesystem failure outside `SQLite` (root directory creation).
    Io(std::io::Error),
    /// `SQLite` cannot provide WAL/FULL durability on this platform.
    DurabilityUnavailable {
        journal_mode: String,
        synchronous: i64,
    },
    /// The stored schema version is unknown to this build.
    SchemaVersionUnsupported(i64),
    /// A durable invariant is violated: a lost CAS, a binding mismatch, a
    /// decode failure.
    CorruptRecord(&'static str),
    /// The authority writer lock is poisoned.
    LockPoisoned,
    /// The artifact authority has no verified package receipt with this id
    /// (the FINALIZED gate: nothing is installed from an unverified or
    /// unknown package reference).
    PackageVerificationReceiptNotFound(ReceiptId),
    /// Another artifact-authority failure during the verified-receipt
    /// readback.
    Artifact(ArtifactError),
    /// The application exists but is disabled; reinstalling it requires the
    /// update/uninstall policy engine that is out of scope for this slice.
    ApplicationDisabled { application_id: ApplicationId },
    /// The application exists but is uninstalled; no lifecycle command can
    /// mutate it further in this slice.
    ApplicationUninstalled { application_id: ApplicationId },
    /// No application exists under this package identity (nothing was ever
    /// installed, so there is nothing to disable).
    ApplicationNotFound { package_id: PackageId },
    /// The application is already disabled and a *different* disable
    /// command (a fresh idempotency key) was issued. The durable disable
    /// receipt of the first command is the only fact; replay is keyed, so
    /// a distinct command against the terminal state is a typed refusal.
    ApplicationAlreadyDisabled { application_id: ApplicationId },
    /// The application is already uninstalled and a *different* uninstall
    /// command (a fresh idempotency key) was issued. The durable uninstall
    /// receipt of the first command is the only fact; replay is keyed, so
    /// a distinct command against the terminal state is a typed refusal.
    ApplicationAlreadyUninstalled { application_id: ApplicationId },
    /// The requested disable timestamp precedes the current installation's
    /// install timestamp (the application row's last update).
    DisablePrecedesInstallation {
        installed_at_ms: u64,
        disabled_at_ms: u64,
    },
    /// The requested uninstall timestamp precedes the current application
    /// row's last update.
    UninstallPrecedesLastUpdate {
        last_updated_at_ms: u64,
        uninstalled_at_ms: u64,
    },
    /// The same idempotency key was reused with a different request shape
    /// (a different verification receipt or a different timestamp).
    IdempotencyConflict,
    /// The requested installation timestamp precedes the package's
    /// verification timestamp (digest binding #7).
    InstallationPrecedesVerification {
        verified_at_ms: u64,
        installed_at_ms: u64,
    },
    /// No installation receipt with this identity exists.
    InstallationNotFound(InstallationId),
    /// The verified receipt's package identity does not match the update
    /// request's named package.
    PackageIdentityMismatch {
        expected: PackageId,
        actual: PackageId,
    },
    /// The verified package's manifest digest matches the application's
    /// current installation; content updates require a changed manifest.
    UpdateManifestUnchanged {
        package_id: PackageId,
        manifest_digest: ContentDigest,
    },
    /// Rollback requires the application to be `disabled` or `uninstalled`;
    /// an installed application cannot roll back in this slice.
    RollbackRequiresDisabledOrUninstalled {
        application_id: ApplicationId,
        status: ApplicationStatus,
    },
    /// The application is already at the initial installation generation;
    /// there is no previous generation to restore.
    RollbackAtInitialGeneration {
        application_id: ApplicationId,
        generation: Generation,
    },
    /// The requested rollback timestamp precedes the application row's
    /// last update.
    RollbackPrecedesLastUpdate {
        last_updated_at_ms: u64,
        rollback_at_ms: u64,
    },
    /// The durable installation history has no receipt for the previous
    /// generation — a corrupt or incomplete record.
    PreviousInstallationNotFound {
        application_id: ApplicationId,
        generation: Generation,
    },
}

impl fmt::Display for ApplicationAuthorityError {
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => {
                write!(formatter, "SQLite application authority failure: {error}")
            }
            Self::Io(error) => {
                write!(formatter, "application authority I/O failure: {error}")
            }
            Self::DurabilityUnavailable {
                journal_mode,
                synchronous,
            } => write!(
                formatter,
                "WAL/FULL durability unavailable: journal_mode={journal_mode}, synchronous={synchronous}"
            ),
            Self::SchemaVersionUnsupported(version) => write!(
                formatter,
                "unsupported application authority schema version {version}"
            ),
            Self::CorruptRecord(reason) => {
                write!(formatter, "corrupt application record: {reason}")
            }
            Self::LockPoisoned => {
                formatter.write_str("application authority writer lock is poisoned")
            }
            Self::PackageVerificationReceiptNotFound(receipt_id) => write!(
                formatter,
                "no verified package receipt {receipt_id:?}; authority-first installs \
                 reference verification facts only by receipt id"
            ),
            Self::Artifact(error) => {
                write!(formatter, "artifact authority readback failure: {error}")
            }
            Self::ApplicationDisabled { application_id } => write!(
                formatter,
                "application {application_id:?} is disabled; reinstall/update \
                 require the policy engine that is out of scope for this slice"
            ),
            Self::ApplicationUninstalled { application_id } => write!(
                formatter,
                "application {application_id:?} is uninstalled; no further \
                 lifecycle commands are accepted in this slice"
            ),
            Self::ApplicationNotFound { package_id } => write!(
                formatter,
                "no application exists under package {package_id:?}; \
                 nothing was ever installed"
            ),
            Self::ApplicationAlreadyDisabled { application_id } => write!(
                formatter,
                "application {application_id:?} is already disabled; replay the \
                 original disable idempotency key instead of issuing a new command"
            ),
            Self::ApplicationAlreadyUninstalled { application_id } => write!(
                formatter,
                "application {application_id:?} is already uninstalled; replay the \
                 original uninstall idempotency key instead of issuing a new command"
            ),
            Self::DisablePrecedesInstallation {
                installed_at_ms,
                disabled_at_ms,
            } => write!(
                formatter,
                "disable timestamp {disabled_at_ms} precedes the current \
                 installation timestamp {installed_at_ms}"
            ),
            Self::UninstallPrecedesLastUpdate {
                last_updated_at_ms,
                uninstalled_at_ms,
            } => write!(
                formatter,
                "uninstall timestamp {uninstalled_at_ms} precedes the current \
                 application update timestamp {last_updated_at_ms}"
            ),
            Self::IdempotencyConflict => formatter
                .write_str("idempotency key reused with a different installation request shape"),
            Self::InstallationPrecedesVerification {
                verified_at_ms,
                installed_at_ms,
            } => write!(
                formatter,
                "installation timestamp {installed_at_ms} precedes verification \
                 timestamp {verified_at_ms}"
            ),
            Self::InstallationNotFound(installation_id) => {
                write!(formatter, "no installation receipt {installation_id:?}")
            }
            Self::PackageIdentityMismatch { expected, actual } => write!(
                formatter,
                "verified package identity {actual:?} does not match the update \
                 request's package {expected:?}"
            ),
            Self::UpdateManifestUnchanged {
                package_id,
                manifest_digest,
            } => write!(
                formatter,
                "verified package {package_id:?} manifest digest {manifest_digest:?} \
                 is unchanged from the current installation; use install for \
                 same-content reinstall"
            ),
            Self::RollbackRequiresDisabledOrUninstalled {
                application_id,
                status,
            } => write!(
                formatter,
                "application {application_id:?} is {status:?}; rollback requires \
                 disabled or uninstalled status in this slice"
            ),
            Self::RollbackAtInitialGeneration {
                application_id,
                generation,
            } => write!(
                formatter,
                "application {application_id:?} is at installation generation \
                 {generation:?}; there is no previous generation to roll back to"
            ),
            Self::RollbackPrecedesLastUpdate {
                last_updated_at_ms,
                rollback_at_ms,
            } => write!(
                formatter,
                "rollback timestamp {rollback_at_ms} precedes the current \
                 application update timestamp {last_updated_at_ms}"
            ),
            Self::PreviousInstallationNotFound {
                application_id,
                generation,
            } => write!(
                formatter,
                "no installation receipt for application {application_id:?} at \
                 generation {generation:?}"
            ),
        }
    }
}

impl Error for ApplicationAuthorityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Artifact(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for ApplicationAuthorityError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// A single-node durable application/installation authority bound to its
/// own `SQLite` store (`application-authority.db`, WAL/FULL, single
/// writer), with the generation guarded by CAS and DDL triggers.
pub struct ApplicationAuthority {
    connection: Mutex<Connection>,
}

impl ApplicationAuthority {
    /// Opens or creates `<root>/application-authority.db`.
    ///
    /// # Errors
    ///
    /// Fails closed when `SQLite` cannot provide WAL/FULL durability, when
    /// the root directory cannot be created, or when a stored schema
    /// version is unknown.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ApplicationAuthorityError> {
        // A `file:` URI root (fault-injection tests) is not a directory to
        // create; its target directory already exists and Windows rejects
        // the `?`/`:` characters outright.
        if !root.as_ref().to_string_lossy().starts_with("file:") {
            std::fs::create_dir_all(root.as_ref()).map_err(ApplicationAuthorityError::Io)?;
        }
        let mut connection = Connection::open(root.as_ref().join("application-authority.db"))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;

        let journal_mode: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let synchronous: i64 =
            connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") || synchronous != 2 {
            return Err(ApplicationAuthorityError::DurabilityUnavailable {
                journal_mode,
                synchronous,
            });
        }

        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => schema::migrate_v1(&mut connection)?,
            1 | 2 | 3 | schema::SCHEMA_VERSION => {}
            other => return Err(ApplicationAuthorityError::SchemaVersionUnsupported(other)),
        }
        if version < 2 {
            schema::migrate_v2(&mut connection)?;
        }
        if version < 3 {
            schema::migrate_v3(&mut connection)?;
        }
        if version < schema::SCHEMA_VERSION {
            schema::migrate_v4(&mut connection)?;
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Installs one verified package: reads the artifact authority's
    /// verified-package receipt by id (verify-then-commit, authority-first),
    /// then in one `Immediate` transaction commits the immutable
    /// installation receipt and CAS-advances the application's current
    /// installation generation.
    ///
    /// Fail-closed order:
    ///
    /// 1. **Artifact receipt readback** (the FINALIZED gate): the caller
    ///    names the verification fact; the artifact authority owns its
    ///    content. An unknown receipt id is a typed refusal with zero
    ///    durable state — an unverified package can never be installed.
    /// 2. **Replay**: the first durable installation receipt under the
    ///    request's idempotency key is the authority; it replays unchanged
    ///    without advancing the generation. The same key with a different
    ///    request shape (different verification receipt or timestamp) is a
    ///    typed [`ApplicationAuthorityError::IdempotencyConflict`].
    /// 3. **Digest binding (seven equations)**: the committed installation
    ///    row must agree bitwise with the verified receipt on receipt id,
    ///    package id, manifest digest, package version, entry count, and
    ///    installer principal, and its timestamp must not precede the
    ///    verification timestamp.
    /// 4. **Generation CAS**: a fresh package creates its application
    ///    singleton at generation 1; an installed application advances
    ///    exactly one generation under a read-then-write CAS; a disabled
    ///    application is a typed refusal. Receipt insert and generation
    ///    advance share the one transaction (co-life), and a receipt can
    ///    only exist at the current generation (DDL guard).
    ///
    /// # Errors
    ///
    /// Fails closed (zero durable state change) for an unknown or unreadable
    /// verification receipt, an idempotency conflict, an installation that
    /// would precede its own verification, a disabled application, a lost
    /// generation CAS, or any storage failure.
    #[allow(clippy::too_many_lines)]
    pub fn install_application(
        &self,
        artifacts: &ArtifactStore,
        request: InstallApplicationRequest,
    ) -> Result<InstallDecision, ApplicationAuthorityError> {
        let verified = readback_verified_receipt(artifacts, request)?;

        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        // Replay first inside the transaction: the durable receipt is the
        // authority and replays without any further verification.
        if let Some(existing) = load_receipt_by_key(&transaction, request.idempotency_key)? {
            if existing.package_verification_receipt_id != request.package_verification_receipt_id
                || existing.installed_at_ms != request.installed_at_ms
            {
                return Err(ApplicationAuthorityError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(InstallDecision::Replayed(existing));
        }

        if request.installed_at_ms < verified.verified_at_ms {
            return Err(
                ApplicationAuthorityError::InstallationPrecedesVerification {
                    verified_at_ms: verified.verified_at_ms,
                    installed_at_ms: request.installed_at_ms,
                },
            );
        }

        let application = load_application_by_package(&transaction, verified.package_id)?;
        let (application_id, generation) = match application {
            None => {
                let application_id = derive_application_id(verified.package_id);
                transaction.execute(
                    "INSERT INTO applications (
                        application_id, package_id, package_manifest_digest,
                        current_installation_generation, status,
                        created_at_ms, updated_at_ms
                     ) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?5)",
                    params![
                        application_id.as_bytes().as_slice(),
                        verified.package_id.as_bytes().as_slice(),
                        verified.manifest_digest.as_bytes().as_slice(),
                        ApplicationStatus::Installed.encode(),
                        encode_u64(request.installed_at_ms)?,
                    ],
                )?;
                (application_id, Generation::INITIAL)
            }
            Some(view) => match view.status {
                ApplicationStatus::Uninstalled => {
                    return Err(ApplicationAuthorityError::ApplicationUninstalled {
                        application_id: view.application_id,
                    });
                }
                ApplicationStatus::Disabled => {
                    return Err(ApplicationAuthorityError::ApplicationDisabled {
                        application_id: view.application_id,
                    });
                }
                ApplicationStatus::Installed => {
                    let next = view.current_installation_generation.checked_next().ok_or(
                        ApplicationAuthorityError::CorruptRecord(
                            "installation generation space is exhausted",
                        ),
                    )?;
                    let changed = transaction.execute(
                        "UPDATE applications
                         SET current_installation_generation = ?1,
                             package_manifest_digest = ?2,
                             updated_at_ms = ?3
                         WHERE application_id = ?4 AND current_installation_generation = ?5",
                        params![
                            encode_generation(next)?,
                            verified.manifest_digest.as_bytes().as_slice(),
                            encode_u64(request.installed_at_ms)?,
                            view.application_id.as_bytes().as_slice(),
                            encode_generation(view.current_installation_generation)?,
                        ],
                    )?;
                    if changed != 1 {
                        return Err(ApplicationAuthorityError::CorruptRecord(
                            "installation generation CAS lost",
                        ));
                    }
                    (view.application_id, next)
                }
            },
        };

        let receipt = InstallationReceipt {
            installation_id: derive_installation_id(
                request.idempotency_key,
                application_id,
                generation,
            ),
            application_id,
            installation_generation: generation,
            package_id: verified.package_id,
            package_manifest_digest: verified.manifest_digest,
            package_version: verified.package_version,
            entry_count: verified.entry_count,
            package_verification_receipt_id: verified.receipt_id,
            installer_principal: verified.signer,
            idempotency_key: request.idempotency_key,
            installed_at_ms: request.installed_at_ms,
        };
        if let Some(error) = binding_error(&receipt, &verified) {
            return Err(error);
        }
        insert_receipt(&transaction, &receipt)?;
        transaction.commit()?;
        Ok(InstallDecision::Installed(receipt))
    }

    /// Disables one installed application: in one `Immediate` transaction
    /// CAS's the status `installed → disabled` (the generation is left
    /// untouched — the state-machine trigger forbids moving it) and
    /// commits the immutable disable receipt.
    ///
    /// Fail-closed order:
    ///
    /// 1. **Replay**: the first durable disable receipt under the request's
    ///    idempotency key is the authority; it replays unchanged without
    ///    touching the state. The same key with a different request shape
    ///    (a different package or timestamp) is a typed
    ///    [`ApplicationAuthorityError::IdempotencyConflict`].
    /// 2. **State CAS**: the application singleton must exist under the
    ///    request's package identity
    ///    ([`ApplicationAuthorityError::ApplicationNotFound`]) and be
    ///    `installed`; a disabled application refuses a distinct disable
    ///    command with
    ///    [`ApplicationAuthorityError::ApplicationAlreadyDisabled`] (the
    ///    status is terminal; only the original key replays). The update
    ///    predicate re-checks the observed status, a lost CAS is a
    ///    [`ApplicationAuthorityError::CorruptRecord`].
    /// 3. **Temporal binding**: the disable timestamp must not precede the
    ///    current installation's install timestamp (the row's last update).
    /// 4. **Single-transaction commit**: the status update and the receipt
    ///    insert share the one transaction (co-life), and the DDL
    ///    state-bounds guard only accepts a receipt for an application
    ///    already disabled at its current generation.
    ///
    /// # Errors
    ///
    /// Fails closed (zero durable state change) for an unknown package, an
    /// idempotency conflict, a distinct command against an already
    /// disabled application, a disable preceding its own installation, a
    /// lost status CAS, or any storage failure.
    pub fn disable_application(
        &self,
        request: DisableApplicationRequest,
    ) -> Result<DisableDecision, ApplicationAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        // Replay first inside the transaction: the durable disable receipt
        // is the authority and replays without any further inspection.
        if let Some(existing) = load_disable_receipt_by_key(&transaction, request.idempotency_key)?
        {
            if existing.application_id != derive_application_id(request.package_id)
                || existing.disabled_at_ms != request.disabled_at_ms
            {
                return Err(ApplicationAuthorityError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(DisableDecision::Replayed(existing));
        }

        let application = load_application_by_package(&transaction, request.package_id)?.ok_or(
            ApplicationAuthorityError::ApplicationNotFound {
                package_id: request.package_id,
            },
        )?;
        match application.status {
            ApplicationStatus::Uninstalled => {
                return Err(ApplicationAuthorityError::ApplicationUninstalled {
                    application_id: application.application_id,
                });
            }
            ApplicationStatus::Disabled => {
                return Err(ApplicationAuthorityError::ApplicationAlreadyDisabled {
                    application_id: application.application_id,
                });
            }
            ApplicationStatus::Installed => {}
        }

        if request.disabled_at_ms < application.updated_at_ms {
            return Err(ApplicationAuthorityError::DisablePrecedesInstallation {
                installed_at_ms: application.updated_at_ms,
                disabled_at_ms: request.disabled_at_ms,
            });
        }

        let changed = transaction.execute(
            "UPDATE applications SET status = ?1, updated_at_ms = ?2
             WHERE application_id = ?3 AND status = ?4",
            params![
                ApplicationStatus::Disabled.encode(),
                encode_u64(request.disabled_at_ms)?,
                application.application_id.as_bytes().as_slice(),
                ApplicationStatus::Installed.encode(),
            ],
        )?;
        if changed != 1 {
            return Err(ApplicationAuthorityError::CorruptRecord(
                "application status CAS lost",
            ));
        }

        let receipt = DisableReceipt {
            application_id: application.application_id,
            application_generation: application.current_installation_generation,
            idempotency_key: request.idempotency_key,
            disabled_at_ms: request.disabled_at_ms,
        };
        insert_disable_receipt(&transaction, &receipt)?;
        transaction.commit()?;
        Ok(DisableDecision::Disabled(receipt))
    }

    /// Updates one installed application to a new verified package
    /// generation: reads the artifact authority's verified-package receipt
    /// by id (verify-then-commit, authority-first), then in one
    /// `Immediate` transaction commits the immutable installation receipt
    /// and CAS-advances the application's current installation generation.
    ///
    /// Fail-closed order:
    ///
    /// 1. **Artifact receipt readback** (the FINALIZED gate): same as
    ///    install — an unknown receipt id is a typed refusal with zero
    ///    durable state.
    /// 2. **Replay**: the first durable installation receipt under the
    ///    request's idempotency key is the authority; it replays unchanged
    ///    without advancing the generation. The same key with a different
    ///    request shape is a typed
    ///    [`ApplicationAuthorityError::IdempotencyConflict`].
    /// 3. **Update preconditions**: the application singleton must exist
    ///    ([`ApplicationAuthorityError::ApplicationNotFound`]), be
    ///    `installed` ([`ApplicationAuthorityError::ApplicationDisabled`]),
    ///    name the same package identity as the verified receipt
    ///    ([`ApplicationAuthorityError::PackageIdentityMismatch`]), and
    ///    target a manifest digest different from the current installation
    ///    ([`ApplicationAuthorityError::UpdateManifestUnchanged`]).
    /// 4. **Digest binding (seven equations)**: same as install.
    /// 5. **Generation CAS**: advances exactly one generation under a
    ///    read-then-write CAS; receipt insert and generation advance share
    ///    the one transaction (co-life).
    ///
    /// # Errors
    ///
    /// Fails closed (zero durable state change) for an unknown or unreadable
    /// verification receipt, a missing/disabled/unchanged application, an
    /// idempotency conflict, an update that would precede its own
    /// verification, a lost generation CAS, or any storage failure.
    pub fn update_application(
        &self,
        artifacts: &ArtifactStore,
        request: UpdateApplicationRequest,
    ) -> Result<UpdateDecision, ApplicationAuthorityError> {
        let verified = readback_verified_receipt_for_update(artifacts, request)?;

        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(existing) = load_receipt_by_key(&transaction, request.idempotency_key)? {
            if existing.package_verification_receipt_id != request.package_verification_receipt_id
                || existing.installed_at_ms != request.updated_at_ms
            {
                return Err(ApplicationAuthorityError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(UpdateDecision::Replayed(existing));
        }

        if request.updated_at_ms < verified.verified_at_ms {
            return Err(
                ApplicationAuthorityError::InstallationPrecedesVerification {
                    verified_at_ms: verified.verified_at_ms,
                    installed_at_ms: request.updated_at_ms,
                },
            );
        }

        if verified.package_id != request.package_id {
            return Err(ApplicationAuthorityError::PackageIdentityMismatch {
                expected: request.package_id,
                actual: verified.package_id,
            });
        }

        let application = load_application_by_package(&transaction, request.package_id)?.ok_or(
            ApplicationAuthorityError::ApplicationNotFound {
                package_id: request.package_id,
            },
        )?;
        match application.status {
            ApplicationStatus::Uninstalled => {
                return Err(ApplicationAuthorityError::ApplicationUninstalled {
                    application_id: application.application_id,
                });
            }
            ApplicationStatus::Disabled => {
                return Err(ApplicationAuthorityError::ApplicationDisabled {
                    application_id: application.application_id,
                });
            }
            ApplicationStatus::Installed => {}
        }

        if verified.manifest_digest == application.package_manifest_digest {
            return Err(ApplicationAuthorityError::UpdateManifestUnchanged {
                package_id: request.package_id,
                manifest_digest: verified.manifest_digest,
            });
        }

        let next = application
            .current_installation_generation
            .checked_next()
            .ok_or(ApplicationAuthorityError::CorruptRecord(
                "installation generation space is exhausted",
            ))?;
        let changed = transaction.execute(
            "UPDATE applications
             SET current_installation_generation = ?1,
                 package_manifest_digest = ?2,
                 updated_at_ms = ?3
             WHERE application_id = ?4 AND current_installation_generation = ?5
               AND status = ?6",
            params![
                encode_generation(next)?,
                verified.manifest_digest.as_bytes().as_slice(),
                encode_u64(request.updated_at_ms)?,
                application.application_id.as_bytes().as_slice(),
                encode_generation(application.current_installation_generation)?,
                ApplicationStatus::Installed.encode(),
            ],
        )?;
        if changed != 1 {
            return Err(ApplicationAuthorityError::CorruptRecord(
                "update generation CAS lost",
            ));
        }

        let receipt = InstallationReceipt {
            installation_id: derive_installation_id(
                request.idempotency_key,
                application.application_id,
                next,
            ),
            application_id: application.application_id,
            installation_generation: next,
            package_id: verified.package_id,
            package_manifest_digest: verified.manifest_digest,
            package_version: verified.package_version,
            entry_count: verified.entry_count,
            package_verification_receipt_id: verified.receipt_id,
            installer_principal: verified.signer,
            idempotency_key: request.idempotency_key,
            installed_at_ms: request.updated_at_ms,
        };
        if let Some(error) = binding_error(&receipt, &verified) {
            return Err(error);
        }
        insert_receipt(&transaction, &receipt)?;
        transaction.commit()?;
        Ok(UpdateDecision::Updated(receipt))
    }

    /// Uninstalls one installed or disabled application: in one `Immediate`
    /// transaction CAS's the status to `uninstalled` (the generation is
    /// left untouched — the state-machine trigger forbids moving it) and
    /// commits the immutable uninstall receipt.
    ///
    /// Fail-closed order:
    ///
    /// 1. **Replay**: the first durable uninstall receipt under the
    ///    request's idempotency key is the authority; it replays unchanged
    ///    without touching the state. The same key with a different request
    ///    shape (a different package or timestamp) is a typed
    ///    [`ApplicationAuthorityError::IdempotencyConflict`].
    /// 2. **State CAS**: the application singleton must exist under the
    ///    request's package identity
    ///    ([`ApplicationAuthorityError::ApplicationNotFound`]) and be
    ///    `installed` or `disabled`; an uninstalled application refuses a
    ///    distinct uninstall command with
    ///    [`ApplicationAuthorityError::ApplicationAlreadyUninstalled`]
    ///    (the status is terminal; only the original key replays). The
    ///    update predicate re-checks the observed status, a lost CAS is a
    ///    [`ApplicationAuthorityError::CorruptRecord`].
    /// 3. **Temporal binding**: the uninstall timestamp must not precede
    ///    the application row's last update (install, update, or disable).
    /// 4. **Single-transaction commit**: the status update and the receipt
    ///    insert share the one transaction (co-life), and the DDL
    ///    state-bounds guard only accepts a receipt for an application
    ///    already uninstalled at its current generation.
    ///
    /// This slice does **not** stop running Tasks/Processes, revoke
    /// Capabilities, garbage-collect artifacts, or physically delete rows.
    ///
    /// # Errors
    ///
    /// Fails closed (zero durable state change) for an unknown package, an
    /// idempotency conflict, a distinct command against an already
    /// uninstalled application, an uninstall preceding its own last update,
    /// a lost status CAS, or any storage failure.
    pub fn uninstall_application(
        &self,
        request: UninstallApplicationRequest,
    ) -> Result<UninstallDecision, ApplicationAuthorityError> {
        self.uninstall_application_internal(request, None)
    }

    pub fn uninstall_application_with_activity_gate(
        &self,
        request: UninstallApplicationRequest,
        probe: &impl ActiveTaskActivityProbe,
    ) -> Result<UninstallDecision, ApplicationAuthorityError> {
        self.uninstall_application_internal(request, Some(probe))
    }

    fn uninstall_application_internal(
        &self,
        request: UninstallApplicationRequest,
        probe: Option<&dyn ActiveTaskActivityProbe>,
    ) -> Result<UninstallDecision, ApplicationAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(existing) =
            load_uninstall_receipt_by_key(&transaction, request.idempotency_key)?
        {
            if existing.application_id != derive_application_id(request.package_id)
                || existing.uninstalled_at_ms != request.uninstalled_at_ms
            {
                return Err(ApplicationAuthorityError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(UninstallDecision::Replayed(existing));
        }

        let application = load_application_by_package(&transaction, request.package_id)?.ok_or(
            ApplicationAuthorityError::ApplicationNotFound {
                package_id: request.package_id,
            },
        )?;
        match application.status {
            ApplicationStatus::Uninstalled => {
                return Err(ApplicationAuthorityError::ApplicationAlreadyUninstalled {
                    application_id: application.application_id,
                });
            }
            ApplicationStatus::Installed | ApplicationStatus::Disabled => {}
        }

        if request.uninstalled_at_ms < application.updated_at_ms {
            return Err(ApplicationAuthorityError::UninstallPrecedesLastUpdate {
                last_updated_at_ms: application.updated_at_ms,
                uninstalled_at_ms: request.uninstalled_at_ms,
            });
        }

        let changed = transaction.execute(
            "UPDATE applications SET status = ?1, updated_at_ms = ?2
             WHERE application_id = ?3 AND status IN (?4, ?5)",
            params![
                ApplicationStatus::Uninstalled.encode(),
                encode_u64(request.uninstalled_at_ms)?,
                application.application_id.as_bytes().as_slice(),
                ApplicationStatus::Installed.encode(),
                ApplicationStatus::Disabled.encode(),
            ],
        )?;
        if changed != 1 {
            return Err(ApplicationAuthorityError::CorruptRecord(
                "application uninstall status CAS lost",
            ));
        }

        let receipt = UninstallReceipt {
            application_id: application.application_id,
            application_generation: application.current_installation_generation,
            idempotency_key: request.idempotency_key,
            uninstalled_at_ms: request.uninstalled_at_ms,
        };
        insert_uninstall_receipt(&transaction, &receipt)?;
        transaction.commit()?;
        Ok(UninstallDecision::Uninstalled(receipt))
    }

    /// Rolls one disabled or uninstalled application back one installation
    /// generation: in one `Immediate` transaction CAS's the status to
    /// `installed`, decrements the generation by exactly one, restores the
    /// previous generation's manifest digest from durable installation
    /// history, and commits the immutable rollback receipt.
    ///
    /// Fail-closed order:
    ///
    /// 1. **Replay**: the first durable rollback receipt under the
    ///    request's idempotency key is the authority; it replays unchanged
    ///    without touching the state. The same key with a different request
    ///    shape (a different package or timestamp) is a typed
    ///    [`ApplicationAuthorityError::IdempotencyConflict`].
    /// 2. **State gate**: the application singleton must exist
    ///    ([`ApplicationAuthorityError::ApplicationNotFound`]) and be
    ///    `disabled` or `uninstalled` ([`ApplicationAuthorityError::
    ///    RollbackRequiresDisabledOrUninstalled`]); an `installed`
    ///    application cannot roll back in this slice.
    /// 3. **Temporal binding**: the rollback timestamp must not precede
    ///    the application row's last update.
    /// 4. **Generation gate**: the current generation must be strictly
    ///    greater than the initial generation ([`ApplicationAuthorityError::
    ///    RollbackAtInitialGeneration`]); the previous installation receipt
    ///    must exist in durable history ([`ApplicationAuthorityError::
    ///    PreviousInstallationNotFound`]).
    /// 5. **Single-transaction commit**: the status/generation/digest update
    ///    and the receipt insert share the one transaction (co-life), and
    ///    the DDL state-bounds guard only accepts a receipt for an
    ///    application already installed at the target generation.
    ///
    /// This slice does **not** implement health checks, migration
    /// compatibility, binary atomic switching, or any full `[PKG-UPDATE-001]`
    /// policy engine beyond the one-step generation anchor.
    ///
    /// # Errors
    ///
    /// Fails closed (zero durable state change) for an unknown package, an
    /// installed application, an initial-generation application, a missing
    /// previous installation receipt, an idempotency conflict, a rollback
    /// preceding its own last update, a lost generation CAS, or any storage
    /// failure.
    pub fn rollback_application(
        &self,
        request: RollbackApplicationRequest,
    ) -> Result<RollbackDecision, ApplicationAuthorityError> {
        self.rollback_application_internal(request, None)
    }

    pub fn rollback_application_with_activity_gate(
        &self,
        request: RollbackApplicationRequest,
        probe: &impl ActiveTaskActivityProbe,
    ) -> Result<RollbackDecision, ApplicationAuthorityError> {
        self.rollback_application_internal(request, Some(probe))
    }

    fn rollback_application_internal(
        &self,
        request: RollbackApplicationRequest,
        probe: Option<&dyn ActiveTaskActivityProbe>,
    ) -> Result<RollbackDecision, ApplicationAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(existing) = load_rollback_receipt_by_key(&transaction, request.idempotency_key)?
        {
            if existing.application_id != derive_application_id(request.package_id)
                || existing.rollback_at_ms != request.rollback_at_ms
            {
                return Err(ApplicationAuthorityError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(RollbackDecision::Replayed(existing));
        }

        let application = load_application_by_package(&transaction, request.package_id)?.ok_or(
            ApplicationAuthorityError::ApplicationNotFound {
                package_id: request.package_id,
            },
        )?;
        match application.status {
            ApplicationStatus::Installed => {
                return Err(
                    ApplicationAuthorityError::RollbackRequiresDisabledOrUninstalled {
                        application_id: application.application_id,
                        status: application.status,
                    },
                );
            }
            ApplicationStatus::Disabled | ApplicationStatus::Uninstalled => {}
        }

        if request.rollback_at_ms < application.updated_at_ms {
            return Err(ApplicationAuthorityError::RollbackPrecedesLastUpdate {
                last_updated_at_ms: application.updated_at_ms,
                rollback_at_ms: request.rollback_at_ms,
            });
        }

        let to_generation = generation_prev(application.current_installation_generation).ok_or(
            ApplicationAuthorityError::RollbackAtInitialGeneration {
                application_id: application.application_id,
                generation: application.current_installation_generation,
            },
        )?;
        let previous = load_installation_receipt_at_generation(
            &transaction,
            application.application_id,
            to_generation,
        )?
        .ok_or(ApplicationAuthorityError::PreviousInstallationNotFound {
            application_id: application.application_id,
            generation: to_generation,
        })?;

        if let Some(probe) = probe {
            let active_task_count = probe.outstanding_task_count(request.package_id);
            if active_task_count > 0 {
                return Err(ApplicationAuthorityError::ApplicationActiveTasksRunning {
                    package_id: request.package_id,
                    active_task_count,
                });
            }
        }

        let changed = transaction.execute(
            "UPDATE applications
             SET status = ?1,
                 current_installation_generation = ?2,
                 package_manifest_digest = ?3,
                 updated_at_ms = ?4
             WHERE application_id = ?5
               AND current_installation_generation = ?6
               AND status IN (?7, ?8)",
            params![
                ApplicationStatus::Installed.encode(),
                encode_generation(to_generation)?,
                previous.package_manifest_digest.as_bytes().as_slice(),
                encode_u64(request.rollback_at_ms)?,
                application.application_id.as_bytes().as_slice(),
                encode_generation(application.current_installation_generation)?,
                ApplicationStatus::Disabled.encode(),
                ApplicationStatus::Uninstalled.encode(),
            ],
        )?;
        if changed != 1 {
            return Err(ApplicationAuthorityError::CorruptRecord(
                "application rollback generation CAS lost",
            ));
        }

        let receipt = RollbackReceipt {
            application_id: application.application_id,
            from_generation: application.current_installation_generation,
            to_generation,
            idempotency_key: request.idempotency_key,
            rollback_at_ms: request.rollback_at_ms,
        };
        insert_rollback_receipt(&transaction, &receipt)?;
        transaction.commit()?;
        Ok(RollbackDecision::RolledBack(receipt))
    }

    /// Reads an application's current durable state by package identity
    /// without any durable side effect. `None` means the package has never
    /// been installed — a legitimate read outcome, not an error.
    ///
    /// # Errors
    ///
    /// Fails closed on a storage error or a corrupt status value.
    pub fn inspect_application(
        &self,
        package_id: PackageId,
    ) -> Result<Option<ApplicationView>, ApplicationAuthorityError> {
        let connection = self.lock()?;
        load_application_by_package(&connection, package_id)
    }

    /// Reads the immutable disable receipt of one application by package
    /// identity. `None` means the application was never disabled — a
    /// legitimate read outcome, not an error.
    ///
    /// # Errors
    ///
    /// Fails closed on a storage error.
    pub fn inspect_disable_receipt(
        &self,
        package_id: PackageId,
    ) -> Result<Option<DisableReceipt>, ApplicationAuthorityError> {
        let connection = self.lock()?;
        load_disable_receipt_by_package(&connection, package_id)
    }

    /// Reads the immutable uninstall receipt of one application by package
    /// identity. `None` means the application was never uninstalled — a
    /// legitimate read outcome, not an error.
    ///
    /// # Errors
    ///
    /// Fails closed on a storage error.
    pub fn inspect_uninstall_receipt(
        &self,
        package_id: PackageId,
    ) -> Result<Option<UninstallReceipt>, ApplicationAuthorityError> {
        let connection = self.lock()?;
        load_uninstall_receipt_by_package(&connection, package_id)
    }

    /// Reads one immutable rollback receipt by idempotency key. `None` means
    /// no rollback was ever recorded under this key — a legitimate read
    /// outcome, not an error.
    ///
    /// # Errors
    ///
    /// Fails closed on a storage error.
    pub fn inspect_rollback_receipt(
        &self,
        idempotency_key: IdempotencyKey,
    ) -> Result<Option<RollbackReceipt>, ApplicationAuthorityError> {
        let connection = self.lock()?;
        load_rollback_receipt_by_key(&connection, idempotency_key)
    }

    /// Lists every immutable rollback receipt of one application, oldest
    /// target generation first. An unknown application lists as empty.
    ///
    /// # Errors
    ///
    /// Fails closed on a storage error.
    pub fn list_rollback_receipts(
        &self,
        application_id: ApplicationId,
    ) -> Result<Vec<RollbackReceipt>, ApplicationAuthorityError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT application_id, from_generation, to_generation,
                    idempotency_key, rollback_at_ms
             FROM application_rollback_receipts WHERE application_id = ?1
             ORDER BY to_generation ASC",
        )?;
        let mut rows = statement.query([application_id.as_bytes().as_slice()])?;
        let mut receipts = Vec::new();
        while let Some(row) = rows.next()? {
            receipts.push(decode_rollback_receipt_row(row)?);
        }
        Ok(receipts)
    }

    /// Reads one immutable installation receipt by installation id.
    ///
    /// # Errors
    ///
    /// Returns [`ApplicationAuthorityError::InstallationNotFound`] or a
    /// storage error.
    pub fn inspect_installation(
        &self,
        installation_id: InstallationId,
    ) -> Result<InstallationReceipt, ApplicationAuthorityError> {
        let connection = self.lock()?;
        load_receipt_optional(&connection, installation_id)?.ok_or(
            ApplicationAuthorityError::InstallationNotFound(installation_id),
        )
    }

    /// Lists every immutable installation receipt of one application,
    /// oldest generation first. An unknown application lists as empty.
    ///
    /// # Errors
    ///
    /// Fails closed on a storage error.
    pub fn list_installations(
        &self,
        application_id: ApplicationId,
    ) -> Result<Vec<InstallationReceipt>, ApplicationAuthorityError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT installation_id, idempotency_key, application_id,
                    installation_generation, package_id, package_manifest_digest,
                    package_version, entry_count, package_verification_receipt_id,
                    installer_principal, installed_at_ms
             FROM installation_receipts WHERE application_id = ?1
             ORDER BY installation_generation ASC",
        )?;
        let mut rows = statement.query([application_id.as_bytes().as_slice()])?;
        let mut receipts = Vec::new();
        while let Some(row) = rows.next()? {
            receipts.push(decode_receipt_row(row)?);
        }
        Ok(receipts)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, ApplicationAuthorityError> {
        self.connection
            .lock()
            .map_err(|_| ApplicationAuthorityError::LockPoisoned)
    }
}

/// The FINALIZED gate: the artifact authority's immutable verified receipt
/// is the only accepted verification fact. There is no non-final receipt
/// state (a receipt exists whole or not at all), so the gate is exactly the
/// readback; a missing receipt is a typed refusal before any durable write.
fn readback_verified_receipt(
    artifacts: &ArtifactStore,
    request: InstallApplicationRequest,
) -> Result<PackageVerificationReceipt, ApplicationAuthorityError> {
    readback_verified_receipt_by_id(artifacts, request.package_verification_receipt_id)
}

fn readback_verified_receipt_for_update(
    artifacts: &ArtifactStore,
    request: UpdateApplicationRequest,
) -> Result<PackageVerificationReceipt, ApplicationAuthorityError> {
    readback_verified_receipt_by_id(artifacts, request.package_verification_receipt_id)
}

fn readback_verified_receipt_by_id(
    artifacts: &ArtifactStore,
    receipt_id: ReceiptId,
) -> Result<PackageVerificationReceipt, ApplicationAuthorityError> {
    artifacts
        .inspect_package_verification_receipt(receipt_id)
        .map_err(|error| match error {
            ArtifactError::PackageVerificationReceiptNotFound(id) => {
                ApplicationAuthorityError::PackageVerificationReceiptNotFound(id)
            }
            other => ApplicationAuthorityError::Artifact(other),
        })
}

/// The seven-equation digest binding between the installation row this
/// authority is about to commit and the artifact authority's verified
/// receipt (mirroring the resource cost-receipt binding precedent). The
/// first six are bitwise equalities on fields copied from the receipt; the
/// seventh is the temporal order `installed_at_ms >= verified_at_ms`.
fn binding_error(
    receipt: &InstallationReceipt,
    verified: &PackageVerificationReceipt,
) -> Option<ApplicationAuthorityError> {
    if receipt.package_verification_receipt_id != verified.receipt_id {
        return Some(ApplicationAuthorityError::CorruptRecord(
            "installation receipt id binding mismatch",
        ));
    }
    if receipt.package_id != verified.package_id {
        return Some(ApplicationAuthorityError::CorruptRecord(
            "installation package id binding mismatch",
        ));
    }
    if receipt.package_manifest_digest != verified.manifest_digest {
        return Some(ApplicationAuthorityError::CorruptRecord(
            "installation manifest digest binding mismatch",
        ));
    }
    if receipt.package_version != verified.package_version {
        return Some(ApplicationAuthorityError::CorruptRecord(
            "installation package version binding mismatch",
        ));
    }
    if receipt.entry_count != verified.entry_count {
        return Some(ApplicationAuthorityError::CorruptRecord(
            "installation entry count binding mismatch",
        ));
    }
    if receipt.installer_principal != verified.signer {
        return Some(ApplicationAuthorityError::CorruptRecord(
            "installation installer principal binding mismatch",
        ));
    }
    if receipt.installed_at_ms < verified.verified_at_ms {
        return Some(
            ApplicationAuthorityError::InstallationPrecedesVerification {
                verified_at_ms: verified.verified_at_ms,
                installed_at_ms: receipt.installed_at_ms,
            },
        );
    }
    None
}

/// Derives the stable application identity of one package identity
/// (domain-separated, mirroring the artifact receipt-id derivation).
#[must_use]
pub fn derive_application_id(package_id: PackageId) -> ApplicationId {
    let mut hasher = Sha256::new();
    hasher.update(APPLICATION_ID_DOMAIN);
    hasher.update(package_id.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    ApplicationId::from_bytes(bytes)
}

/// Derives the installation identity of one idempotent install command:
/// domain-separated over the idempotency key, the application identity, and
/// the generation it installs, so a redone command after a lost commit
/// converges onto exactly the same installation id.
#[must_use]
pub fn derive_installation_id(
    idempotency_key: IdempotencyKey,
    application_id: ApplicationId,
    generation: Generation,
) -> InstallationId {
    let mut hasher = Sha256::new();
    hasher.update(INSTALLATION_ID_DOMAIN);
    hasher.update(idempotency_key.as_bytes());
    hasher.update(application_id.as_bytes());
    hasher.update(generation.get().to_be_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    InstallationId::from_bytes(bytes)
}

fn load_application_by_package(
    source: &Connection,
    package_id: PackageId,
) -> Result<Option<ApplicationView>, ApplicationAuthorityError> {
    let mut statement = source.prepare(
        "SELECT application_id, package_manifest_digest,
                current_installation_generation, status,
                created_at_ms, updated_at_ms
         FROM applications WHERE package_id = ?1",
    )?;
    let mut rows = statement.query([package_id.as_bytes().as_slice()])?;
    rows.next()?
        .map(|row| decode_application_row(row, package_id))
        .transpose()
}

fn decode_application_row(
    row: &rusqlite::Row<'_>,
    package_id: PackageId,
) -> Result<ApplicationView, ApplicationAuthorityError> {
    Ok(ApplicationView {
        application_id: ApplicationId::from_bytes(blob16(row, 0)?),
        package_id,
        package_manifest_digest: ContentDigest::from_bytes(blob32(row, 1)?),
        current_installation_generation: decode_generation(row, 2)?,
        status: ApplicationStatus::decode(row.get(3)?)?,
        created_at_ms: decode_u64(row, 4)?,
        updated_at_ms: decode_u64(row, 5)?,
    })
}

fn load_receipt_by_key(
    source: &Connection,
    key: IdempotencyKey,
) -> Result<Option<InstallationReceipt>, ApplicationAuthorityError> {
    let mut statement = source.prepare(
        "SELECT installation_id, idempotency_key, application_id,
                installation_generation, package_id, package_manifest_digest,
                package_version, entry_count, package_verification_receipt_id,
                installer_principal, installed_at_ms
         FROM installation_receipts WHERE idempotency_key = ?1",
    )?;
    let mut rows = statement.query([key.as_bytes().as_slice()])?;
    rows.next()?.map(decode_receipt_row).transpose()
}

fn load_receipt_optional(
    source: &Connection,
    installation_id: InstallationId,
) -> Result<Option<InstallationReceipt>, ApplicationAuthorityError> {
    let mut statement = source.prepare(
        "SELECT installation_id, idempotency_key, application_id,
                installation_generation, package_id, package_manifest_digest,
                package_version, entry_count, package_verification_receipt_id,
                installer_principal, installed_at_ms
         FROM installation_receipts WHERE installation_id = ?1",
    )?;
    let mut rows = statement.query([installation_id.as_bytes().as_slice()])?;
    rows.next()?.map(decode_receipt_row).transpose()
}

fn load_disable_receipt_by_key(
    source: &Connection,
    key: IdempotencyKey,
) -> Result<Option<DisableReceipt>, ApplicationAuthorityError> {
    let mut statement = source.prepare(
        "SELECT application_id, application_generation, idempotency_key, disabled_at_ms
         FROM application_disable_receipts WHERE idempotency_key = ?1",
    )?;
    let mut rows = statement.query([key.as_bytes().as_slice()])?;
    rows.next()?.map(decode_disable_receipt_row).transpose()
}

fn load_disable_receipt_by_package(
    source: &Connection,
    package_id: PackageId,
) -> Result<Option<DisableReceipt>, ApplicationAuthorityError> {
    let mut statement = source.prepare(
        "SELECT application_id, application_generation, idempotency_key, disabled_at_ms
         FROM application_disable_receipts
         WHERE application_id = (SELECT application_id FROM applications
                                 WHERE package_id = ?1)",
    )?;
    let mut rows = statement.query([package_id.as_bytes().as_slice()])?;
    rows.next()?.map(decode_disable_receipt_row).transpose()
}

fn load_uninstall_receipt_by_key(
    source: &Connection,
    key: IdempotencyKey,
) -> Result<Option<UninstallReceipt>, ApplicationAuthorityError> {
    let mut statement = source.prepare(
        "SELECT application_id, application_generation, idempotency_key, uninstalled_at_ms
         FROM application_uninstall_receipts WHERE idempotency_key = ?1",
    )?;
    let mut rows = statement.query([key.as_bytes().as_slice()])?;
    rows.next()?.map(decode_uninstall_receipt_row).transpose()
}

fn load_uninstall_receipt_by_package(
    source: &Connection,
    package_id: PackageId,
) -> Result<Option<UninstallReceipt>, ApplicationAuthorityError> {
    let mut statement = source.prepare(
        "SELECT application_id, application_generation, idempotency_key, uninstalled_at_ms
         FROM application_uninstall_receipts
         WHERE application_id = (SELECT application_id FROM applications
                                 WHERE package_id = ?1)",
    )?;
    let mut rows = statement.query([package_id.as_bytes().as_slice()])?;
    rows.next()?.map(decode_uninstall_receipt_row).transpose()
}

fn load_rollback_receipt_by_key(
    source: &Connection,
    key: IdempotencyKey,
) -> Result<Option<RollbackReceipt>, ApplicationAuthorityError> {
    let mut statement = source.prepare(
        "SELECT application_id, from_generation, to_generation,
                idempotency_key, rollback_at_ms
         FROM application_rollback_receipts WHERE idempotency_key = ?1",
    )?;
    let mut rows = statement.query([key.as_bytes().as_slice()])?;
    rows.next()?.map(decode_rollback_receipt_row).transpose()
}

fn load_installation_receipt_at_generation(
    source: &Connection,
    application_id: ApplicationId,
    generation: Generation,
) -> Result<Option<InstallationReceipt>, ApplicationAuthorityError> {
    let mut statement = source.prepare(
        "SELECT installation_id, idempotency_key, application_id,
                installation_generation, package_id, package_manifest_digest,
                package_version, entry_count, package_verification_receipt_id,
                installer_principal, installed_at_ms
         FROM installation_receipts
         WHERE application_id = ?1 AND installation_generation = ?2",
    )?;
    let mut rows = statement.query(params![
        application_id.as_bytes().as_slice(),
        encode_generation(generation)?,
    ])?;
    rows.next()?.map(decode_receipt_row).transpose()
}

fn insert_rollback_receipt(
    transaction: &Connection,
    receipt: &RollbackReceipt,
) -> Result<(), ApplicationAuthorityError> {
    transaction.execute(
        "INSERT INTO application_rollback_receipts (
            idempotency_key, application_id, from_generation, to_generation,
            rollback_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            receipt.idempotency_key.as_bytes().as_slice(),
            receipt.application_id.as_bytes().as_slice(),
            encode_generation(receipt.from_generation)?,
            encode_generation(receipt.to_generation)?,
            encode_u64(receipt.rollback_at_ms)?,
        ],
    )?;
    Ok(())
}

fn decode_rollback_receipt_row(
    row: &rusqlite::Row<'_>,
) -> Result<RollbackReceipt, ApplicationAuthorityError> {
    Ok(RollbackReceipt {
        application_id: ApplicationId::from_bytes(blob16(row, 0)?),
        from_generation: decode_generation(row, 1)?,
        to_generation: decode_generation(row, 2)?,
        idempotency_key: IdempotencyKey::from_bytes(blob16(row, 3)?),
        rollback_at_ms: decode_u64(row, 4)?,
    })
}

fn generation_prev(current: Generation) -> Option<Generation> {
    let prev = current.get().checked_sub(1)?;
    std::num::NonZeroU64::new(prev).map(Generation::new)
}

fn insert_uninstall_receipt(
    transaction: &Connection,
    receipt: &UninstallReceipt,
) -> Result<(), ApplicationAuthorityError> {
    transaction.execute(
        "INSERT INTO application_uninstall_receipts (
            application_id, idempotency_key, application_generation, uninstalled_at_ms
         ) VALUES (?1, ?2, ?3, ?4)",
        params![
            receipt.application_id.as_bytes().as_slice(),
            receipt.idempotency_key.as_bytes().as_slice(),
            encode_generation(receipt.application_generation)?,
            encode_u64(receipt.uninstalled_at_ms)?,
        ],
    )?;
    Ok(())
}

fn decode_uninstall_receipt_row(
    row: &rusqlite::Row<'_>,
) -> Result<UninstallReceipt, ApplicationAuthorityError> {
    Ok(UninstallReceipt {
        application_id: ApplicationId::from_bytes(blob16(row, 0)?),
        application_generation: decode_generation(row, 1)?,
        idempotency_key: IdempotencyKey::from_bytes(blob16(row, 2)?),
        uninstalled_at_ms: decode_u64(row, 3)?,
    })
}

fn insert_disable_receipt(
    transaction: &Connection,
    receipt: &DisableReceipt,
) -> Result<(), ApplicationAuthorityError> {
    transaction.execute(
        "INSERT INTO application_disable_receipts (
            application_id, idempotency_key, application_generation, disabled_at_ms
         ) VALUES (?1, ?2, ?3, ?4)",
        params![
            receipt.application_id.as_bytes().as_slice(),
            receipt.idempotency_key.as_bytes().as_slice(),
            encode_generation(receipt.application_generation)?,
            encode_u64(receipt.disabled_at_ms)?,
        ],
    )?;
    Ok(())
}

fn decode_disable_receipt_row(
    row: &rusqlite::Row<'_>,
) -> Result<DisableReceipt, ApplicationAuthorityError> {
    Ok(DisableReceipt {
        application_id: ApplicationId::from_bytes(blob16(row, 0)?),
        application_generation: decode_generation(row, 1)?,
        idempotency_key: IdempotencyKey::from_bytes(blob16(row, 2)?),
        disabled_at_ms: decode_u64(row, 3)?,
    })
}

fn insert_receipt(
    transaction: &Connection,
    receipt: &InstallationReceipt,
) -> Result<(), ApplicationAuthorityError> {
    transaction.execute(
        "INSERT INTO installation_receipts (
            installation_id, idempotency_key, application_id,
            installation_generation, package_id, package_manifest_digest,
            package_version, entry_count, package_verification_receipt_id,
            installer_principal, installed_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            receipt.installation_id.as_bytes().as_slice(),
            receipt.idempotency_key.as_bytes().as_slice(),
            receipt.application_id.as_bytes().as_slice(),
            encode_generation(receipt.installation_generation)?,
            receipt.package_id.as_bytes().as_slice(),
            receipt.package_manifest_digest.as_bytes().as_slice(),
            encode_u64(receipt.package_version)?,
            encode_u64(receipt.entry_count)?,
            receipt
                .package_verification_receipt_id
                .as_bytes()
                .as_slice(),
            receipt.installer_principal.as_bytes().as_slice(),
            encode_u64(receipt.installed_at_ms)?,
        ],
    )?;
    Ok(())
}

fn decode_receipt_row(
    row: &rusqlite::Row<'_>,
) -> Result<InstallationReceipt, ApplicationAuthorityError> {
    Ok(InstallationReceipt {
        installation_id: InstallationId::from_bytes(blob16(row, 0)?),
        idempotency_key: IdempotencyKey::from_bytes(blob16(row, 1)?),
        application_id: ApplicationId::from_bytes(blob16(row, 2)?),
        installation_generation: decode_generation(row, 3)?,
        package_id: PackageId::from_bytes(blob16(row, 4)?),
        package_manifest_digest: ContentDigest::from_bytes(blob32(row, 5)?),
        package_version: decode_u64(row, 6)?,
        entry_count: decode_u64(row, 7)?,
        package_verification_receipt_id: ReceiptId::from_bytes(blob16(row, 8)?),
        installer_principal: PrincipalId::from_bytes(blob16(row, 9)?),
        installed_at_ms: decode_u64(row, 10)?,
    })
}

fn encode_u64(value: u64) -> Result<i64, ApplicationAuthorityError> {
    i64::try_from(value)
        .map_err(|_| ApplicationAuthorityError::CorruptRecord("u64 column exceeds SQLite i64"))
}

fn decode_u64(row: &rusqlite::Row<'_>, index: usize) -> Result<u64, ApplicationAuthorityError> {
    let value: i64 = row.get(index)?;
    u64::try_from(value)
        .map_err(|_| ApplicationAuthorityError::CorruptRecord("negative u64 column"))
}

fn encode_generation(value: Generation) -> Result<i64, ApplicationAuthorityError> {
    encode_u64(value.get())
}

fn decode_generation(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> Result<Generation, ApplicationAuthorityError> {
    let value = decode_u64(row, index)?;
    let nonzero = std::num::NonZeroU64::new(value).ok_or(
        ApplicationAuthorityError::CorruptRecord("zero installation generation"),
    )?;
    Ok(Generation::new(nonzero))
}

fn blob_n<const N: usize>(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> Result<[u8; N], ApplicationAuthorityError> {
    let bytes: Vec<u8> = row.get(index)?;
    <[u8; N]>::try_from(bytes.as_slice())
        .map_err(|_| ApplicationAuthorityError::CorruptRecord("application blob length mismatch"))
}

fn blob16(row: &rusqlite::Row<'_>, index: usize) -> Result<[u8; 16], ApplicationAuthorityError> {
    blob_n(row, index)
}

fn blob32(row: &rusqlite::Row<'_>, index: usize) -> Result<[u8; 32], ApplicationAuthorityError> {
    blob_n(row, index)
}

#[cfg(test)]
mod tests {
    use nlos_types::{ApplicationId, InstallationId, PackageId};

    use super::{ApplicationStatus, derive_application_id, derive_installation_id};
    use crate::{ApplicationAuthorityError, InstallApplicationRequest, InstallationReceipt};
    use nlos_artifact::PackageVerificationReceipt;
    use nlos_types::{Generation, IdempotencyKey, ReceiptId};

    fn request(receipt_seed: u8, key_seed: u8, at_ms: u64) -> InstallApplicationRequest {
        InstallApplicationRequest {
            package_verification_receipt_id: ReceiptId::from_bytes([receipt_seed; 16]),
            idempotency_key: IdempotencyKey::from_bytes([key_seed; 16]),
            installed_at_ms: at_ms,
        }
    }

    /// The derivation precedents: distinct domains and inputs yield distinct
    /// identities, and the same inputs are deterministic.
    #[test]
    fn authority_derived_ids_are_domain_separated_and_deterministic() {
        let package = PackageId::from_bytes([0x11; 16]);
        let other = PackageId::from_bytes([0x22; 16]);

        let application = derive_application_id(package);
        assert_eq!(application, derive_application_id(package), "deterministic");
        assert_ne!(derive_application_id(other), application);
        assert_ne!(
            derive_application_id(package),
            ApplicationId::from_bytes(package.into_bytes()),
            "the derivation is domain-separated, not a byte copy"
        );

        let installation = derive_installation_id(
            IdempotencyKey::from_bytes([0x33; 16]),
            application,
            Generation::INITIAL,
        );
        assert_eq!(
            installation,
            derive_installation_id(
                IdempotencyKey::from_bytes([0x33; 16]),
                application,
                Generation::INITIAL,
            )
        );
        assert_ne!(installation, InstallationId::from_bytes([0x33; 16]));
        assert_ne!(
            derive_installation_id(
                IdempotencyKey::from_bytes([0x44; 16]),
                application,
                Generation::INITIAL
            ),
            installation,
            "a distinct install command derives a distinct installation"
        );
    }

    /// Status encodings are the values the DDL `CHECK` constraints accept;
    /// anything else is a corrupt record.
    #[test]
    fn status_encoding_round_trips_and_rejects_unknowns() {
        assert_eq!(ApplicationStatus::Installed.encode(), 1);
        assert_eq!(ApplicationStatus::Disabled.encode(), 2);
        assert_eq!(ApplicationStatus::Uninstalled.encode(), 3);
        assert_eq!(
            ApplicationStatus::decode(ApplicationStatus::Installed.encode()).expect("installed"),
            ApplicationStatus::Installed
        );
        assert_eq!(
            ApplicationStatus::decode(ApplicationStatus::Disabled.encode()).expect("disabled"),
            ApplicationStatus::Disabled
        );
        assert_eq!(
            ApplicationStatus::decode(ApplicationStatus::Uninstalled.encode())
                .expect("uninstalled"),
            ApplicationStatus::Uninstalled
        );
        assert!(matches!(
            ApplicationStatus::decode(4),
            Err(ApplicationAuthorityError::CorruptRecord(_))
        ));
        assert!(matches!(
            ApplicationStatus::decode(0),
            Err(ApplicationAuthorityError::CorruptRecord(_))
        ));
    }

    /// Every request field participates: the idempotency key, the referenced
    /// verification receipt, and the timestamp are the whole request shape.
    #[test]
    fn request_shape_is_fully_captured() {
        let first = request(0x01, 0x02, 5_000);
        let same = request(0x01, 0x02, 5_000);
        assert_eq!(
            first.package_verification_receipt_id,
            same.package_verification_receipt_id
        );
        assert_eq!(first.idempotency_key, same.idempotency_key);
        assert_eq!(first.installed_at_ms, same.installed_at_ms);
        assert_ne!(request(0x03, 0x02, 5_000), first);
        assert_ne!(request(0x01, 0x04, 5_000), first);
        assert_ne!(request(0x01, 0x02, 6_000), first);
    }

    /// The binding guard rejects each broken equation in turn (installation
    /// precedes verification is its own typed variant).
    #[test]
    fn binding_guard_is_exact() {
        let verified = PackageVerificationReceipt {
            receipt_id: ReceiptId::from_bytes([0x01; 16]),
            manifest_digest: nlos_artifact::ContentDigest::of_bytes(b"manifest"),
            package_id: PackageId::from_bytes([0x02; 16]),
            package_version: 7,
            entry_count: 3,
            signer: nlos_types::PrincipalId::from_bytes([0x03; 16]),
            key_id: nlos_types::KeyId::from_bytes([0x04; 16]),
            key_generation: Generation::INITIAL,
            signature: [0xAA; 64],
            verified_at_ms: 1_000,
        };
        let receipt = InstallationReceipt {
            installation_id: InstallationId::from_bytes([0x05; 16]),
            application_id: derive_application_id(verified.package_id),
            installation_generation: Generation::INITIAL,
            package_id: verified.package_id,
            package_manifest_digest: verified.manifest_digest,
            package_version: verified.package_version,
            entry_count: verified.entry_count,
            package_verification_receipt_id: verified.receipt_id,
            installer_principal: verified.signer,
            idempotency_key: IdempotencyKey::from_bytes([0x06; 16]),
            installed_at_ms: 2_000,
        };
        assert!(super::binding_error(&receipt, &verified).is_none(), "bound");

        let mut broken = receipt.clone();
        broken.package_verification_receipt_id = ReceiptId::from_bytes([0x0F; 16]);
        assert!(matches!(
            super::binding_error(&broken, &verified),
            Some(ApplicationAuthorityError::CorruptRecord(
                "installation receipt id binding mismatch"
            ))
        ));

        let mut broken = receipt.clone();
        broken.package_id = PackageId::from_bytes([0x0F; 16]);
        assert!(matches!(
            super::binding_error(&broken, &verified),
            Some(ApplicationAuthorityError::CorruptRecord(
                "installation package id binding mismatch"
            ))
        ));

        let mut broken = receipt.clone();
        broken.package_manifest_digest = nlos_artifact::ContentDigest::of_bytes(b"other");
        assert!(matches!(
            super::binding_error(&broken, &verified),
            Some(ApplicationAuthorityError::CorruptRecord(
                "installation manifest digest binding mismatch"
            ))
        ));

        let mut broken = receipt.clone();
        broken.package_version = 8;
        assert!(matches!(
            super::binding_error(&broken, &verified),
            Some(ApplicationAuthorityError::CorruptRecord(
                "installation package version binding mismatch"
            ))
        ));

        let mut broken = receipt.clone();
        broken.entry_count = 4;
        assert!(matches!(
            super::binding_error(&broken, &verified),
            Some(ApplicationAuthorityError::CorruptRecord(
                "installation entry count binding mismatch"
            ))
        ));

        let mut broken = receipt.clone();
        broken.installer_principal = nlos_types::PrincipalId::from_bytes([0x0F; 16]);
        assert!(matches!(
            super::binding_error(&broken, &verified),
            Some(ApplicationAuthorityError::CorruptRecord(
                "installation installer principal binding mismatch"
            ))
        ));

        let mut broken = receipt.clone();
        broken.installed_at_ms = 1_000 - 1;
        assert!(matches!(
            super::binding_error(&broken, &verified),
            Some(
                ApplicationAuthorityError::InstallationPrecedesVerification {
                    verified_at_ms: 1_000,
                    installed_at_ms: 999,
                }
            )
        ));
    }
}
