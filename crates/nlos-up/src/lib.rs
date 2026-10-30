//! Single-node NLOS developer trial bootstrap orchestrator and cross-platform launcher.
//!
//! `nlos-up` closes the local "zero-to-trial" startup loop across Windows (Named Pipes)
//! and Unix (domain sockets) without inventing new authority semantics:
//!
//! 1. **Idempotent state-root bootstrap** ([`bootstrap_trial_root`]):
//!    - resolves the trial root (`--root`, `LLMOS_HOME`, `USERPROFILE`/`HOME`, or `temp_dir`);
//!    - generates or reuses a 64-lowercase-hex `Ed25519` developer trial seed at
//!      `<root>/keys/operator-ed25519.key` (owner-only `0o600` on Unix; local developer
//!      trial seed only, never for production key custody);
//!    - bootstraps (or replays) the matching operator principal in `<root>/identity` using
//!      the exact derivation shared with `nlos_system_control::daemon`, then drops the
//!      `SQLite` handle;
//!    - seeds (or replays) a default trial [`AccountRecord`] and [`LlmTokenBudgetRecord`]
//!      in `<root>/resource`, records `<root>/trial-account.txt`, and immediately drops
//!      the [`ResourceAuthority`] handle so [`nlos_system_control::daemon::assemble`] can
//!      open `<root>` with zero lock contention;
//!    - optionally pre-installs `examples/sample-app/package` (or a caller-supplied sample
//!      package directory) into `<root>/artifacts` + `<root>/application` and drops all
//!      handles before returning;
//!    - performs a lightweight secret-free probe for `LLMOS_KEYPOOL_CONFIG` or
//!      `~/glm-keypool/config.json`, extracting only the loopback listening port while
//!      never storing or printing `egressKey` or `keys`.
//! 2. **Session environment projection** ([`resolve_desktop_env`]):
//!    - computes `auth_socket_path`, `plain_socket_path`, and `principal_hex` identically
//!      to `DaemonOptions`, rendering ready-to-paste `PowerShell` or POSIX shell blocks.
//! 3. **Embedded daemon & desktop launcher helpers** ([`daemon_options_for_receipt`]).

use std::error::Error;
use std::fmt::{self, Write as _};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer as _, SigningKey};
use nlos_application::{
    ApplicationAuthority, ApplicationAuthorityError, InstallApplicationRequest, InstallDecision,
};
use nlos_artifact::{
    ArtifactError, ArtifactStore, ContentDigest, CreateArtifactSpec, PackageEntryRole,
    PackageManifest, PackageManifestEntry, PutRevisionRequest, SignedPackage, VerifyPackageRequest,
    package_manifest_message,
};
use nlos_driver_llm_http::{
    HttpUrl, KeyPoolCompiledIntent, KeyPoolHealthSnapshot, LlmChatRequest, LlmDriverError,
    LlmEnforcementShim, MeteredBudgetContext, MeteredLlmChatOutcome,
    PROVIDER_ISOLATION_UNVERIFIED_TAG, current_wall_ms, derive_idempotency_key,
    derive_operation_id, discover_local_keypool, format_receipt_id_hex,
};
use nlos_identity::{
    BootstrapDecision, BootstrapPrincipalRequest, IdentityAuthority, IdentityAuthorityError,
    KeyPurpose,
};
use nlos_resource::{
    AccountRecord, CreateAccountRequest, CreateLlmTokenBudgetRequest, LlmTokenBudgetDecision,
    LlmTokenBudgetRecord, LlmTokenLimits, ResourceAuthority, ResourceAuthorityError,
};
use nlos_slice_k::provenance_triple;
use nlos_system_control::daemon::{DaemonError, DaemonOptions};
use nlos_types::{ArtifactId, IdempotencyKey, PackageId, PrincipalId, ResourceAccountId};
use sha2::{Digest, Sha256};

/// Canonical Cargo package identifier for the `nlos-up` bootstrap crate.
pub const CRATE_NAME: &str = "nlos-up";

/// Relative path under `<root>` holding the 64-hex `Ed25519` developer trial key seed.
pub const OPERATOR_KEY_REL_PATH: &str = "keys/operator-ed25519.key";
/// File under `<root>` recording the 32-hex trial `ResourceAccountId`.
pub const TRIAL_ACCOUNT_FILE: &str = "trial-account.txt";
/// File under `<root>` recording the 32-hex bootstrapped operator `PrincipalId`.
pub const TRIAL_PRINCIPAL_FILE: &str = "trial-principal.txt";

/// Default initial credit seeded for the local trial [`AccountRecord`].
pub const DEFAULT_TRIAL_INITIAL_CREDIT: u64 = 10_000_000;
/// Default prompt-token ceiling for the local trial [`LlmTokenBudgetRecord`].
pub const DEFAULT_TRIAL_MAX_PROMPT_TOKENS: u64 = 5_000_000;
/// Default completion-token ceiling for the local trial [`LlmTokenBudgetRecord`].
pub const DEFAULT_TRIAL_MAX_COMPLETION_TOKENS: u64 = 5_000_000;
/// Default total-token ceiling for the local trial [`LlmTokenBudgetRecord`].
pub const DEFAULT_TRIAL_MAX_TOTAL_TOKENS: u64 = 10_000_000;
/// Default model API call ceiling for the local trial [`LlmTokenBudgetRecord`].
pub const DEFAULT_TRIAL_MAX_API_CALLS: u64 = 10_000;
/// Default cost ceiling (in microcredits) for the local trial [`LlmTokenBudgetRecord`].
pub const DEFAULT_TRIAL_MAX_COST_MICROCREDITS: u64 = 10_000_000;

/// Default `glm-keypool` loopback port when `config.json` omits an explicit `"port"` field.
pub const DEFAULT_KEYPOOL_PORT: u16 = 8317;

/// Key validity ceiling matching `nlos_system_control::daemon` (2100-01-01 UTC).
const BOOTSTRAP_KEY_VALID_UNTIL_MS: u64 = 4_102_444_800_000;

const TRIAL_ACCOUNT_KEY_DOMAIN: &[u8] = b"llmos/nlos-up/trial-account-idempotency/v1";
const TRIAL_BUDGET_KEY_DOMAIN: &[u8] = b"llmos/nlos-up/trial-llm-budget-idempotency/v1";
const SAMPLE_APP_KEY_DOMAIN: &[u8] = b"llmos/nlos-up/sample-app-seed/v1";
const DEFAULT_SAMPLE_PACKAGE_ID: [u8; 16] = [
    0x9a, 0x1c, 0x7d, 0x3e, 0x5f, 0x0b, 0x2a, 0x4c, 0x6d, 0x8e, 0x0f, 0x1a, 0x2b, 0x3c, 0x4d, 0x5e,
];

/// Typed failure surface for `nlos-up` bootstrap, environment projection, and launcher flows.
#[derive(Debug)]
pub enum UpError {
    /// Filesystem I/O failure.
    Io(std::io::Error),
    /// Operator key file is malformed (must be 64 lowercase ASCII hex characters).
    InvalidKeyFile(String),
    /// Unix-only: operator key file permissions are wider than `0o600`.
    InsecureKeyMode(u32),
    /// `IdentityAuthority` rejected principal bootstrap.
    Identity(Box<IdentityAuthorityError>),
    /// `ResourceAuthority` rejected trial account or LLM budget seeding.
    Resource(Box<ResourceAuthorityError>),
    /// `ArtifactStore` rejected sample package materialization or verification.
    Artifact(Box<ArtifactError>),
    /// `ApplicationAuthority` rejected sample package installation.
    Application(Box<ApplicationAuthorityError>),
    /// `SystemControlDaemon` assembly failed.
    Daemon(Box<DaemonError>),
    /// Local `KeyPool` HTTP LLM driver or budget accounting failure.
    LlmDriver(Box<LlmDriverError>),
    /// CLI or option validation error.
    InvalidArgument(String),
    /// Child desktop dev process failed to launch or exited unsuccessfully.
    DesktopProcess(String),
}

impl From<std::io::Error> for UpError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<IdentityAuthorityError> for UpError {
    fn from(error: IdentityAuthorityError) -> Self {
        Self::Identity(Box::new(error))
    }
}

impl From<ResourceAuthorityError> for UpError {
    fn from(error: ResourceAuthorityError) -> Self {
        Self::Resource(Box::new(error))
    }
}

impl From<ArtifactError> for UpError {
    fn from(error: ArtifactError) -> Self {
        Self::Artifact(Box::new(error))
    }
}

impl From<ApplicationAuthorityError> for UpError {
    fn from(error: ApplicationAuthorityError) -> Self {
        Self::Application(Box::new(error))
    }
}

impl From<DaemonError> for UpError {
    fn from(error: DaemonError) -> Self {
        Self::Daemon(Box::new(error))
    }
}

impl From<LlmDriverError> for UpError {
    fn from(error: LlmDriverError) -> Self {
        Self::LlmDriver(Box::new(error))
    }
}

impl fmt::Display for UpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "i/o failure: {error}"),
            Self::InvalidKeyFile(reason) => {
                write!(formatter, "invalid operator key file: {reason}")
            }
            Self::InsecureKeyMode(mode) => {
                write!(
                    formatter,
                    "operator key file mode must be 0600 on Unix (found {mode:04o})"
                )
            }
            Self::Identity(error) => write!(formatter, "identity bootstrap failure: {error}"),
            Self::Resource(error) => write!(formatter, "resource seed failure: {error}"),
            Self::Artifact(error) => write!(formatter, "sample package artifact failure: {error}"),
            Self::Application(error) => {
                write!(formatter, "sample package install failure: {error}")
            }
            Self::Daemon(error) => write!(formatter, "daemon assembly failure: {error}"),
            Self::LlmDriver(error) => write!(formatter, "llm driver failure: {error}"),
            Self::InvalidArgument(reason) => write!(formatter, "invalid argument: {reason}"),
            Self::DesktopProcess(reason) => write!(formatter, "desktop launcher failure: {reason}"),
        }
    }
}

impl Error for UpError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Identity(error) => Some(error.as_ref()),
            Self::Resource(error) => Some(error.as_ref()),
            Self::Artifact(error) => Some(error.as_ref()),
            Self::Application(error) => Some(error.as_ref()),
            Self::Daemon(error) => Some(error.as_ref()),
            Self::LlmDriver(error) => Some(error.as_ref()),
            Self::InvalidKeyFile(_)
            | Self::InsecureKeyMode(_)
            | Self::InvalidArgument(_)
            | Self::DesktopProcess(_) => None,
        }
    }
}

/// Target shell syntax for `nlos-up env`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellKind {
    /// Windows `PowerShell` (`$env:KEY = "VALUE"`).
    PowerShell,
    /// POSIX `sh`/`bash`/`zsh` (`export KEY='VALUE'`).
    Posix,
}

impl ShellKind {
    /// Returns the host platform's default shell syntax (`PowerShell` on Windows, `Posix` elsewhere).
    #[must_use]
    pub const fn host_default() -> Self {
        if cfg!(windows) {
            Self::PowerShell
        } else {
            Self::Posix
        }
    }

    /// Parses a `--shell` flag value (`powershell` or `posix`).
    ///
    /// # Errors
    ///
    /// Returns [`UpError::InvalidArgument`] when `raw` is not a recognized shell name.
    pub fn parse(raw: &str) -> Result<Self, UpError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "powershell" | "pwsh" => Ok(Self::PowerShell),
            "posix" | "sh" | "bash" | "zsh" => Ok(Self::Posix),
            other => Err(UpError::InvalidArgument(format!(
                "unsupported shell '{other}' (expected 'powershell' or 'posix')"
            ))),
        }
    }
}

/// Provenance of the operator `Ed25519` trial seed file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyOrigin {
    /// Freshly generated local developer trial seed (`dev-trial-only`).
    GeneratedDevTrial,
    /// Existing 64-lowercase-hex seed file validated and reused idempotently.
    ReusedExisting,
}

/// Outcome of the optional sample application pre-installation step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SampleAppSeedStatus {
    /// Sample app seeding was not requested or no sample package directory was present.
    Skipped,
    /// Sample package was freshly materialized, verified, and installed into `<root>/application`.
    Installed {
        package_id_hex: String,
        application_id_hex: String,
    },
    /// Sample package was already installed in `<root>/application` and preserved idempotently.
    AlreadyInstalled {
        package_id_hex: String,
        application_id_hex: String,
    },
}

/// Secret-free structural summary of a detected local `glm-keypool` configuration file.
///
/// By construction, this type holds **only** the configuration file path and the loopback
/// `http://127.0.0.1:<port>` endpoint URL; `egressKey`, `keys`, and any other credentials
/// are never parsed into memory structures, stored, or printed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeypoolProbeSummary {
    /// Path to the detected `config.json`.
    pub config_path: PathBuf,
    /// Loopback listening port extracted from `"port"` (or [`DEFAULT_KEYPOOL_PORT`]).
    pub listen_port: u16,
    /// Loopback base URL (`http://127.0.0.1:<port>`).
    pub endpoint_url: String,
}

/// Configuration options for [`bootstrap_trial_root`] and [`resolve_desktop_env`].
#[derive(Clone, Debug)]
pub struct BootstrapOptions {
    /// Optional explicit state root override (`--root <DIR>`).
    pub root: Option<PathBuf>,
    /// When `true`, pre-installs `examples/sample-app/package` (or `sample_package_dir`) if found.
    pub seed_sample_app: bool,
    /// Optional explicit sample package directory (`--sample-package-dir <DIR>`).
    pub sample_package_dir: Option<PathBuf>,
    /// Optional explicit `glm-keypool` config path override (takes precedence over env/home).
    pub keypool_config_override: Option<PathBuf>,
    /// Whether to probe `LLMOS_KEYPOOL_CONFIG` and `~/glm-keypool/config.json` when no override is set.
    pub probe_default_keypool: bool,
}

impl Default for BootstrapOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl BootstrapOptions {
    /// Creates default bootstrap options (`~/.llmos/trial-state`, keypool probe enabled, clean root).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            root: None,
            seed_sample_app: false,
            sample_package_dir: None,
            keypool_config_override: None,
            probe_default_keypool: true,
        }
    }

    /// Overrides the trial state root directory.
    #[must_use]
    pub fn with_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = Some(root.into());
        self
    }

    /// Enables or disables automatic sample package seeding.
    #[must_use]
    pub const fn with_seed_sample_app(mut self, enabled: bool) -> Self {
        self.seed_sample_app = enabled;
        self
    }

    /// Sets an explicit sample package directory and enables sample package seeding.
    #[must_use]
    pub fn with_sample_package_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.sample_package_dir = Some(dir.into());
        self.seed_sample_app = true;
        self
    }

    /// Sets an explicit `glm-keypool` config path to probe instead of host defaults.
    #[must_use]
    pub fn with_keypool_config(mut self, path: impl Into<PathBuf>) -> Self {
        self.keypool_config_override = Some(path.into());
        self
    }

    /// Disables probing host environment/home for `glm-keypool` (used by isolated automated tests).
    #[must_use]
    pub const fn without_default_keypool_probe(mut self) -> Self {
        self.probe_default_keypool = false;
        self
    }

    /// Resolves the target state root directory using `--root`, `LLMOS_HOME`, `USERPROFILE`/`HOME`, or `temp_dir`.
    #[must_use]
    pub fn resolved_root(&self) -> PathBuf {
        self.root.clone().unwrap_or_else(default_trial_root)
    }
}

/// Receipt returned by [`bootstrap_trial_root`] after all authorities have been seeded and closed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BootstrapReceipt {
    /// Resolved state root directory.
    pub root: PathBuf,
    /// Path to `<root>/keys/operator-ed25519.key`.
    pub key_file: PathBuf,
    /// Whether the developer trial key was freshly generated or reused.
    pub key_origin: KeyOrigin,
    /// 64-lowercase-hex `Ed25519` verifying (public) key.
    pub public_key_hex: String,
    /// Bootstrapped operator `PrincipalId`.
    pub principal_id: PrincipalId,
    /// 32-lowercase-hex representation of `principal_id`.
    pub principal_hex: String,
    /// `true` when the principal binding was replayed from an existing identity store.
    pub principal_replayed: bool,
    /// Default trial `ResourceAccountId`.
    pub account_id: ResourceAccountId,
    /// 32-lowercase-hex representation of `account_id`.
    pub account_id_hex: String,
    /// Durable [`AccountRecord`] for the trial account.
    pub account_record: AccountRecord,
    /// Durable [`LlmTokenBudgetRecord`] for the trial account.
    pub llm_budget: LlmTokenBudgetRecord,
    /// `true` when the LLM token budget was replayed idempotently.
    pub llm_budget_replayed: bool,
    /// Path to `<root>/resource`.
    pub resource_root: PathBuf,
    /// Path to `<root>/application`.
    pub application_root: PathBuf,
    /// Derived authenticated IPC endpoint path (Unix socket or Windows Named Pipe).
    pub auth_socket_path: PathBuf,
    /// Derived plain CLI IPC endpoint path (Unix socket or Windows Named Pipe).
    pub plain_socket_path: PathBuf,
    /// Sample application installation status.
    pub sample_app: SampleAppSeedStatus,
    /// Secret-free `glm-keypool` detection summary, if found.
    pub keypool: Option<KeypoolProbeSummary>,
}

/// Complete desktop & CLI session environment variables matching [`SystemControlDaemon`](nlos_system_control::daemon::SystemControlDaemon).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopSessionEnv {
    /// State root directory.
    pub root: PathBuf,
    /// Authenticated IPC endpoint (`LLMOS_DESKTOP_SOCKET`).
    pub auth_socket_path: PathBuf,
    /// Bootstrapped operator principal (`LLMOS_DESKTOP_PRINCIPAL`, 32 hex).
    pub principal_hex: String,
    /// Operator `Ed25519` seed file (`LLMOS_DESKTOP_KEY_FILE`).
    pub key_file: PathBuf,
    /// Plain CLI IPC endpoint (`LLMOS_DESKTOP_CLI_SOCKET`).
    pub plain_socket_path: PathBuf,
    /// Optional `system-control-cli` binary path (`LLMOS_DESKTOP_CLI`).
    pub cli_binary_path: Option<PathBuf>,
    /// Resource authority directory (`LLMOS_DESKTOP_RESOURCE_ROOT`).
    pub resource_root: PathBuf,
    /// Application authority directory (`LLMOS_DESKTOP_APPLICATION_ROOT`).
    pub application_root: PathBuf,
    /// Seeded trial resource account (`32` hex, informational / helper export).
    pub account_id_hex: String,
    /// Secret-free `glm-keypool` probe result (`LLMOS_KEYPOOL_CONFIG` when present).
    pub keypool: Option<KeypoolProbeSummary>,
}

impl DesktopSessionEnv {
    /// Builds [`DesktopSessionEnv`] from a completed [`BootstrapReceipt`].
    #[must_use]
    pub fn from_receipt(receipt: &BootstrapReceipt) -> Self {
        Self {
            root: receipt.root.clone(),
            auth_socket_path: receipt.auth_socket_path.clone(),
            principal_hex: receipt.principal_hex.clone(),
            key_file: receipt.key_file.clone(),
            plain_socket_path: receipt.plain_socket_path.clone(),
            cli_binary_path: detect_sibling_cli_binary(),
            resource_root: receipt.resource_root.clone(),
            application_root: receipt.application_root.clone(),
            account_id_hex: receipt.account_id_hex.clone(),
            keypool: receipt.keypool.clone(),
        }
    }

    /// Returns the ordered `(NAME, VALUE)` environment variable pairs injected into `desktop`.
    #[must_use]
    pub fn entries(&self) -> Vec<(String, String)> {
        let mut vars = vec![
            (
                "LLMOS_DESKTOP_SOCKET".to_owned(),
                self.auth_socket_path.display().to_string(),
            ),
            (
                "LLMOS_DESKTOP_PRINCIPAL".to_owned(),
                self.principal_hex.clone(),
            ),
            (
                "LLMOS_DESKTOP_KEY_FILE".to_owned(),
                self.key_file.display().to_string(),
            ),
            (
                "LLMOS_DESKTOP_CLI_SOCKET".to_owned(),
                self.plain_socket_path.display().to_string(),
            ),
            (
                "LLMOS_DESKTOP_RESOURCE_ROOT".to_owned(),
                self.resource_root.display().to_string(),
            ),
            (
                "LLMOS_DESKTOP_APPLICATION_ROOT".to_owned(),
                self.application_root.display().to_string(),
            ),
        ];
        if let Some(cli) = &self.cli_binary_path {
            vars.push(("LLMOS_DESKTOP_CLI".to_owned(), cli.display().to_string()));
        }
        if let Some(probe) = &self.keypool {
            vars.push((
                "LLMOS_KEYPOOL_CONFIG".to_owned(),
                probe.config_path.display().to_string(),
            ));
        }
        vars
    }

    /// Renders a ready-to-eval shell snippet for `PowerShell` or POSIX shells.
    #[must_use]
    pub fn render_shell(&self, shell: ShellKind) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "# nlos-up session environment (root: {}, trial_account: {})",
            self.root.display(),
            self.account_id_hex
        );
        for (name, value) in self.entries() {
            match shell {
                ShellKind::PowerShell => {
                    let escaped = escape_powershell_double_quoted(&value);
                    let _ = writeln!(out, "$env:{name} = \"{escaped}\"");
                }
                ShellKind::Posix => {
                    let escaped = escape_posix_single_quoted(&value);
                    let _ = writeln!(out, "export {name}='{escaped}'");
                }
            }
        }
        out
    }
}

/// Resolves the default trial state directory:
/// `LLMOS_HOME/trial-state` -> `(USERPROFILE|HOME)/.llmos/trial-state` -> `<temp_dir>/llmos-trial-state`.
#[must_use]
pub fn default_trial_root() -> PathBuf {
    default_trial_root_from_vars(
        std::env::var_os("LLMOS_HOME").as_deref(),
        user_home_dir().as_deref(),
        &std::env::temp_dir(),
    )
}

/// Pure helper for [`default_trial_root`] enabling deterministic unit tests.
#[must_use]
pub fn default_trial_root_from_vars(
    llmos_home: Option<&std::ffi::OsStr>,
    user_home: Option<&Path>,
    temp_dir: &Path,
) -> PathBuf {
    if let Some(custom) = llmos_home.filter(|val| !val.is_empty()) {
        return PathBuf::from(custom).join("trial-state");
    }
    if let Some(home) = user_home {
        return home.join(".llmos").join("trial-state");
    }
    temp_dir.join("llmos-trial-state")
}

fn user_home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let keys = ["USERPROFILE", "HOME"];
    #[cfg(not(windows))]
    let keys = ["HOME", "USERPROFILE"];

    keys.into_iter()
        .find_map(|key| std::env::var_os(key).filter(|val| !val.is_empty()))
        .map(PathBuf::from)
}

/// Derives the default `(auth_socket_path, plain_socket_path)` pair using the exact
/// per-platform rules of `nlos_system_control::daemon::DaemonOptions`.
#[must_use]
pub fn derive_daemon_endpoints(root: &Path) -> (PathBuf, PathBuf) {
    (auth_endpoint_default(root), plain_endpoint_default(root))
}

#[cfg(unix)]
fn auth_endpoint_default(root: &Path) -> PathBuf {
    root.join("system-control-auth.sock")
}

#[cfg(unix)]
fn plain_endpoint_default(root: &Path) -> PathBuf {
    root.join("system-control-plain.sock")
}

#[cfg(windows)]
fn auth_endpoint_default(root: &Path) -> PathBuf {
    default_pipe_endpoint("auth", root)
}

#[cfg(windows)]
fn plain_endpoint_default(root: &Path) -> PathBuf {
    default_pipe_endpoint("plain", root)
}

#[cfg(windows)]
fn default_pipe_endpoint(role: &str, root: &Path) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/system-control-daemon/pipe-name/v1");
    hasher.update(root.as_os_str().as_encoded_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    PathBuf::from(format!(
        r"\\.\pipe\llmos-system-control-{role}-{}",
        encode_hex(&digest[..8])
    ))
}

/// Builds [`DaemonOptions`] wired to the bootstrapped operator identity and ADR-0022 root key.
#[must_use]
pub fn daemon_options_for_receipt(receipt: &BootstrapReceipt) -> DaemonOptions {
    DaemonOptions::new(&receipt.root)
        .with_identity_key_file(&receipt.key_file)
        .with_operator_key_file(&receipt.key_file)
}

/// Idempotently initializes `<root>` for local trial execution and closes all `SQLite` handles
/// before returning so [`nlos_system_control::daemon::assemble`] can immediately open `<root>`.
///
/// # Errors
///
/// Fails closed with [`UpError`] on filesystem permission/format violations or authority errors.
pub fn bootstrap_trial_root(options: &BootstrapOptions) -> Result<BootstrapReceipt, UpError> {
    let root = options.resolved_root();
    let key_file = root.join(OPERATOR_KEY_REL_PATH);
    if let Some(parent) = key_file.parent() {
        fs::create_dir_all(parent).map_err(UpError::Io)?;
    } else {
        fs::create_dir_all(&root).map_err(UpError::Io)?;
    }

    let (seed, key_origin) = load_or_generate_operator_key(&key_file)?;
    let signing_key = SigningKey::from_bytes(&seed);
    let public_key = signing_key.verifying_key().to_bytes();
    let public_key_hex = encode_hex(&public_key);

    // Step 1: Bootstrap (or replay) operator principal in `<root>/identity`, then drop handle.
    let (principal_id, principal_replayed) = bootstrap_operator_principal(&root, public_key)?;
    let principal_hex = encode_hex(principal_id.as_bytes());
    write_marker_file(&root.join(TRIAL_PRINCIPAL_FILE), &principal_hex)?;

    // Step 2: Seed (or replay) default trial AccountRecord & LlmTokenBudgetRecord, then drop handle.
    let resource_root = root.join("resource");
    let (account_record, llm_budget, llm_budget_replayed) =
        seed_trial_resource_budget(&resource_root)?;
    let account_id = account_record.account_id;
    let account_id_hex = encode_hex(account_id.as_bytes());
    write_marker_file(&root.join(TRIAL_ACCOUNT_FILE), &account_id_hex)?;

    // Step 3: Optional sample package pre-installation into `<root>/application`, then drop handles.
    let application_root = root.join("application");
    let sample_app = if options.seed_sample_app || options.sample_package_dir.is_some() {
        seed_sample_app_if_present(
            &root,
            &signing_key,
            principal_id,
            options.sample_package_dir.as_deref(),
        )?
    } else {
        SampleAppSeedStatus::Skipped
    };

    // Step 4: Secret-free local `glm-keypool` structure summary probe.
    let keypool = resolve_keypool_probe(options);
    let (auth_socket_path, plain_socket_path) = derive_daemon_endpoints(&root);

    Ok(BootstrapReceipt {
        root,
        key_file,
        key_origin,
        public_key_hex,
        principal_id,
        principal_hex,
        principal_replayed,
        account_id,
        account_id_hex,
        account_record,
        llm_budget,
        llm_budget_replayed,
        resource_root,
        application_root,
        auth_socket_path,
        plain_socket_path,
        sample_app,
        keypool,
    })
}

/// Resolves [`DesktopSessionEnv`] for `<root>`.
///
/// When `<root>` has already been bootstrapped (e.g. while `nlos-up daemon` is actively
/// running and holding `SQLite` handles), this reads the validated key and marker files
/// without opening `SQLite`; otherwise it runs [`bootstrap_trial_root`] first.
///
/// # Errors
///
/// Propagates [`UpError`] if `<root>` holds a malformed key file or if initial bootstrap fails.
pub fn resolve_desktop_env(options: &BootstrapOptions) -> Result<DesktopSessionEnv, UpError> {
    let root = options.resolved_root();
    let key_file = root.join(OPERATOR_KEY_REL_PATH);
    let principal_file = root.join(TRIAL_PRINCIPAL_FILE);
    let account_file = root.join(TRIAL_ACCOUNT_FILE);

    if key_file.is_file() && principal_file.is_file() && account_file.is_file() {
        let _ = read_existing_lowercase_hex_key(&key_file)?;
        let principal_hex = read_hex16_marker(&principal_file, "trial-principal.txt")?;
        let account_id_hex = read_hex16_marker(&account_file, "trial-account.txt")?;
        let (auth_socket_path, plain_socket_path) = derive_daemon_endpoints(&root);
        return Ok(DesktopSessionEnv {
            auth_socket_path,
            principal_hex,
            key_file,
            plain_socket_path,
            cli_binary_path: detect_sibling_cli_binary(),
            resource_root: root.join("resource"),
            application_root: root.join("application"),
            account_id_hex,
            keypool: resolve_keypool_probe(options),
            root,
        });
    }

    let receipt = bootstrap_trial_root(options)?;
    Ok(DesktopSessionEnv::from_receipt(&receipt))
}

/// Performs a secret-free structural probe on a candidate `glm-keypool` JSON file.
///
/// Returns `None` if `path` does not exist or cannot be read. Extracts only `"port"`
/// (defaulting to [`DEFAULT_KEYPOOL_PORT`]) and constructs `http://127.0.0.1:<port>`.
/// Never extracts, stores, or logs `egressKey`, `keys`, or any credential values.
#[must_use]
pub fn probe_keypool_config_file(path: &Path) -> Option<KeypoolProbeSummary> {
    if !path.is_file() {
        return None;
    }
    let raw = fs::read_to_string(path).ok()?;
    let listen_port = extract_port_from_json(&raw).unwrap_or(DEFAULT_KEYPOOL_PORT);
    drop(raw);
    Some(KeypoolProbeSummary {
        config_path: path.to_path_buf(),
        listen_port,
        endpoint_url: format!("http://127.0.0.1:{listen_port}"),
    })
}

/// Redacted credential marker string emitted by `nlos-up pool-status`.
pub const POOL_STATUS_REDACTED_MARKER: &str = "[REDACTED]";

static NEXT_TRIAL_LLM_NONCE: AtomicU64 = AtomicU64::new(1);

/// Completely redacted local `KeyPool` configuration and `/__pool__/status` probe report
/// (`DRV-CRED-001`, `[PD-COMMIT-004]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalKeyPoolStatusReport {
    /// `true` when a local `KeyPool` configuration source was resolved.
    pub configured: bool,
    /// `true` when `/__pool__/status` responded `2xx` with valid telemetry JSON.
    pub reachable: bool,
    /// Redacted discovery provenance label (never leaks host user home directories).
    pub discovery_source: String,
    /// Loopback gateway origin (e.g. `"http://127.0.0.1:8317"`).
    pub gateway_origin: Option<String>,
    /// Full `OpenAI`-compatible base URL (e.g. `"http://127.0.0.1:8317/api/coding/paas/v4"`).
    pub openai_base_url: Option<String>,
    /// `OpenAI`-compatible path prefix (e.g. `"/api/coding/paas/v4"` or `"/v1"`).
    pub openai_base_path: Option<String>,
    /// Default primary model identifier.
    pub default_model: Option<String>,
    /// Fast low-latency intent compilation model identifier.
    pub fast_intent_model: Option<String>,
    /// Aggregated model allowlist.
    pub allowed_models: Vec<String>,
    /// Count of configured upstream key slots (zero raw keys or nicknames retained).
    pub configured_key_slots: usize,
    /// Boolean flag indicating whether an egress bearer token is configured.
    pub credential_present: bool,
    /// Fixed redaction marker (`"[REDACTED]"`).
    pub credential_marker: &'static str,
    /// Honest `ADR-0024` isolation posture (`"PROVIDER_ISOLATION_UNVERIFIED"`).
    pub isolation_tag: &'static str,
    /// Sanitized `/__pool__/status` aggregate telemetry when reachable.
    pub live_health: Option<KeyPoolHealthSnapshot>,
    /// Privacy-safe reason when `reachable` is `false`.
    pub unreachable_reason: Option<String>,
}

/// Parameters for [`run_trial_metered_chat`] (`nlos-up chat`).
#[derive(Clone, Debug)]
pub struct TrialMeteredChatRequest {
    /// Trial root and optional `--keypool-config` options.
    pub options: BootstrapOptions,
    /// Non-empty single-turn user prompt.
    pub prompt: String,
    /// Optional target model override (`--model <MODEL>`).
    pub model: Option<String>,
    /// Optional `max_tokens` ceiling (`--max-tokens <N>`).
    pub max_tokens: Option<u32>,
    /// Optional sticky session affinity identifier (`--session <ID>`).
    pub session_id: Option<String>,
}

/// Complete outcome of [`run_trial_metered_chat`], pairing the bootstrapped trial root
/// receipt with the 5D-budget-metered [`MeteredLlmChatOutcome`].
#[derive(Clone, Debug)]
pub struct TrialMeteredChatReport {
    /// Bootstrapped (or replayed) trial root receipt.
    pub bootstrap: BootstrapReceipt,
    /// Redacted model reply, slot fingerprint, token usage, and durable consumption receipt.
    pub outcome: MeteredLlmChatOutcome,
    /// 32-lowercase-hex encoding of `outcome.consumption_receipt.receipt_id`.
    pub receipt_id_hex: Option<String>,
}

/// Parameters for [`run_trial_compiled_intent`] (`nlos-up intent`).
#[derive(Clone, Debug)]
pub struct TrialCompiledIntentRequest {
    /// Trial root and optional `--keypool-config` options.
    pub options: BootstrapOptions,
    /// Non-empty natural-language user query (`--query <TEXT>`).
    pub query: String,
    /// Target locale (`zh-CN` or `en-US`).
    pub locale: String,
    /// Optional target model override (`--model <MODEL>`).
    pub model: Option<String>,
}

/// Complete outcome of [`run_trial_compiled_intent`], pairing the bootstrapped trial root
/// receipt with the 5D-budget-metered [`KeyPoolCompiledIntent`].
#[derive(Clone, Debug)]
pub struct TrialCompiledIntentReport {
    /// Bootstrapped (or replayed) trial root receipt.
    pub bootstrap: BootstrapReceipt,
    /// Structured intent plan with `[NL-AMBIG-001]` safety escalation and durable budget receipt.
    pub compiled: KeyPoolCompiledIntent,
    /// 32-lowercase-hex encoding of `compiled.consumption_receipt.receipt_id`.
    pub receipt_id_hex: Option<String>,
}

/// Discovers local `KeyPool` configuration and probes `/__pool__/status` with 100%
/// credential, slot-nickname, and host-path redaction.
///
/// When `require_live` is `false`, an unstarted gateway (or missing default auto-discovery
/// when `keypool_config` is `None`) gracefully reports `reachable = false` with a sanitized
/// reason instead of failing; when `require_live` is `true`, unreachable or missing gateways
/// fail closed with [`UpError::LlmDriver`].
///
/// # Errors
///
/// Fails closed if an explicit `keypool_config` file cannot be read/parsed or if `require_live`
/// is `true` while the local gateway is unconfigured or unreachable.
pub async fn inspect_trial_keypool_status(
    keypool_config: Option<&Path>,
    require_live: bool,
) -> Result<LocalKeyPoolStatusReport, UpError> {
    let discovered = match discover_local_keypool(keypool_config) {
        Ok(cfg) => cfg,
        Err(LlmDriverError::DiscoveryNotFound(reason))
            if keypool_config.is_none() && !require_live =>
        {
            return Ok(LocalKeyPoolStatusReport {
                configured: false,
                reachable: false,
                discovery_source: "unconfigured".to_owned(),
                gateway_origin: None,
                openai_base_url: None,
                openai_base_path: None,
                default_model: None,
                fast_intent_model: None,
                allowed_models: Vec::new(),
                configured_key_slots: 0,
                credential_present: false,
                credential_marker: POOL_STATUS_REDACTED_MARKER,
                isolation_tag: PROVIDER_ISOLATION_UNVERIFIED_TAG,
                live_health: None,
                unreachable_reason: Some(reason.to_owned()),
            });
        }
        Err(error) => return Err(UpError::from(error)),
    };

    let (gateway_origin, openai_base_path) = split_origin_and_path(&discovered.openai_base_url);
    let discovery_source = discovered.source.redacted_label().to_owned();
    let openai_base_url = Some(discovered.openai_base_url.clone());
    let default_model = Some(discovered.default_model.clone());
    let fast_intent_model = Some(discovered.fast_intent_model.clone());
    let allowed_models = discovered.allowed_models.clone();
    let configured_key_slots = discovered.configured_key_slots;
    let credential_present = !discovered.egress_key.is_empty();

    let shim = LlmEnforcementShim::from_discovered(discovered);
    match shim.probe_pool_status().await {
        Ok(snap) => Ok(LocalKeyPoolStatusReport {
            configured: true,
            reachable: true,
            discovery_source,
            gateway_origin,
            openai_base_url,
            openai_base_path,
            default_model,
            fast_intent_model,
            allowed_models,
            configured_key_slots,
            credential_present,
            credential_marker: POOL_STATUS_REDACTED_MARKER,
            isolation_tag: PROVIDER_ISOLATION_UNVERIFIED_TAG,
            live_health: Some(snap),
            unreachable_reason: None,
        }),
        Err(probe_err) => {
            if require_live {
                return Err(UpError::from(probe_err));
            }
            Ok(LocalKeyPoolStatusReport {
                configured: true,
                reachable: false,
                discovery_source,
                gateway_origin,
                openai_base_url,
                openai_base_path,
                default_model,
                fast_intent_model,
                allowed_models,
                configured_key_slots,
                credential_present,
                credential_marker: POOL_STATUS_REDACTED_MARKER,
                isolation_tag: PROVIDER_ISOLATION_UNVERIFIED_TAG,
                live_health: None,
                unreachable_reason: Some(sanitize_llm_error_summary(&probe_err)),
            })
        }
    }
}

/// Bootstraps (or replays) `<root>`, binds the trial `ResourceAccountId` recorded in
/// `<root>/trial-account.txt` and `<root>/resource`, and executes a 5D-budget-metered
/// chat completion via [`LlmEnforcementShim::execute_metered_chat`].
///
/// # Errors
///
/// Fails closed on empty prompt, trial bootstrap failure, `KeyPool` discovery/transport
/// failure, or 5D token budget exhaustion.
pub async fn run_trial_metered_chat(
    request: &TrialMeteredChatRequest,
) -> Result<TrialMeteredChatReport, UpError> {
    let trimmed_prompt = request.prompt.trim();
    if trimmed_prompt.is_empty() {
        return Err(UpError::InvalidArgument(
            "chat requires non-empty --prompt <TEXT>".to_owned(),
        ));
    }

    let bootstrap = bootstrap_trial_root(&request.options)?;
    let account_id = resolve_bootstrapped_trial_account(&bootstrap)?;
    let shim = LlmEnforcementShim::discover(request.options.keypool_config_override.as_deref())?;

    let mut chat_req = LlmChatRequest::single_user_prompt(trimmed_prompt);
    if let Some(model) = request
        .model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        chat_req = chat_req.with_model(model);
    }
    if let Some(max_tokens) = request.max_tokens {
        chat_req = chat_req.with_max_tokens(max_tokens);
    }
    if let Some(session_id) = request
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        chat_req = chat_req.with_session_id(session_id);
    }

    let resource = ResourceAuthority::open(&bootstrap.resource_root)?;
    let budget = resource.inspect_llm_token_budget(account_id)?;
    let budget_ctx = build_trial_budget_context(
        &resource,
        account_id,
        &budget,
        b"llmos/nlos-up/chat-op/v1",
        b"llmos/nlos-up/chat-idem/v1",
    );
    let outcome = shim
        .execute_metered_chat(Some(budget_ctx), &chat_req)
        .await?;
    drop(resource);

    let receipt_id_hex = outcome
        .consumption_receipt
        .map(|r| format_receipt_id_hex(r.receipt_id));
    Ok(TrialMeteredChatReport {
        bootstrap,
        outcome,
        receipt_id_hex,
    })
}

/// Bootstraps (or replays) `<root>`, binds the trial `ResourceAccountId` recorded in
/// `<root>/trial-account.txt` and `<root>/resource`, and compiles a structured NLOS intent
/// via [`LlmEnforcementShim::compile_nl_intent`] / [`LlmEnforcementShim::compile_nl_intent_with_model`].
///
/// # Errors
///
/// Fails closed on empty query, trial bootstrap failure, `KeyPool` discovery/transport
/// failure, malformed JSON completion, or 5D token budget exhaustion.
pub async fn run_trial_compiled_intent(
    request: &TrialCompiledIntentRequest,
) -> Result<TrialCompiledIntentReport, UpError> {
    let trimmed_query = request.query.trim();
    if trimmed_query.is_empty() {
        return Err(UpError::InvalidArgument(
            "intent requires non-empty --query <TEXT>".to_owned(),
        ));
    }

    let bootstrap = bootstrap_trial_root(&request.options)?;
    let account_id = resolve_bootstrapped_trial_account(&bootstrap)?;
    let shim = LlmEnforcementShim::discover(request.options.keypool_config_override.as_deref())?;

    let resource = ResourceAuthority::open(&bootstrap.resource_root)?;
    let budget = resource.inspect_llm_token_budget(account_id)?;
    let budget_ctx = build_trial_budget_context(
        &resource,
        account_id,
        &budget,
        b"llmos/nlos-up/intent-op/v1",
        b"llmos/nlos-up/intent-idem/v1",
    );
    let model_override = request
        .model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let compiled = match model_override {
        Some(model) => {
            shim.compile_nl_intent_with_model(
                Some(budget_ctx),
                trimmed_query,
                &request.locale,
                Some(model),
            )
            .await?
        }
        None => {
            shim.compile_nl_intent(Some(budget_ctx), trimmed_query, &request.locale)
                .await?
        }
    };
    drop(resource);

    let receipt_id_hex = compiled
        .consumption_receipt
        .map(|r| format_receipt_id_hex(r.receipt_id));
    Ok(TrialCompiledIntentReport {
        bootstrap,
        compiled,
        receipt_id_hex,
    })
}

fn resolve_bootstrapped_trial_account(
    bootstrap: &BootstrapReceipt,
) -> Result<ResourceAccountId, UpError> {
    let marker_path = bootstrap.root.join(TRIAL_ACCOUNT_FILE);
    let marker_hex = read_hex16_marker(&marker_path, TRIAL_ACCOUNT_FILE)?;
    let marker_bytes = parse_hex16(&marker_hex, TRIAL_ACCOUNT_FILE)?;
    let account_id = ResourceAccountId::from_bytes(marker_bytes);
    if account_id != bootstrap.account_id {
        return Err(UpError::InvalidArgument(
            "trial-account.txt marker diverged from bootstrapped ResourceAccountId".to_owned(),
        ));
    }
    Ok(account_id)
}

fn build_trial_budget_context<'a>(
    resource: &'a ResourceAuthority,
    account_id: ResourceAccountId,
    budget: &LlmTokenBudgetRecord,
    op_domain: &[u8],
    idem_domain: &[u8],
) -> MeteredBudgetContext<'a> {
    let wall_ms = current_wall_ms();
    let now_ms = wall_ms.max(budget.updated_at_ms);
    let pid_bytes = std::process::id().to_be_bytes();
    let nanos_bytes = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0_u128, |d| d.as_nanos())
        .to_be_bytes();
    let seq_bytes = budget.consumed_api_calls.saturating_add(1).to_be_bytes();
    let nonce_bytes = NEXT_TRIAL_LLM_NONCE
        .fetch_add(1, Ordering::Relaxed)
        .to_be_bytes();
    let parts: [&[u8]; 5] = [
        account_id.as_bytes(),
        &pid_bytes,
        &nanos_bytes,
        &seq_bytes,
        &nonce_bytes,
    ];
    MeteredBudgetContext {
        resource,
        account_id,
        operation_id: derive_operation_id(op_domain, &parts),
        idempotency_key: derive_idempotency_key(idem_domain, &parts),
        now_ms,
    }
}

fn split_origin_and_path(openai_base_url: &str) -> (Option<String>, Option<String>) {
    let Ok(parsed) = HttpUrl::parse(openai_base_url) else {
        return (None, None);
    };
    let host = if parsed.host.contains(':') && !parsed.host.starts_with('[') {
        format!("[{}]", parsed.host)
    } else {
        parsed.host
    };
    let origin = format!("http://{host}:{}", parsed.port);
    (Some(origin), Some(parsed.path_and_query))
}

fn sanitize_llm_error_summary(error: &LlmDriverError) -> String {
    match error {
        LlmDriverError::ConfigIo(io_err) => format!("config file i/o error ({:?})", io_err.kind()),
        other => other.to_string(),
    }
}

fn resolve_keypool_probe(options: &BootstrapOptions) -> Option<KeypoolProbeSummary> {
    if let Some(override_path) = &options.keypool_config_override {
        return probe_keypool_config_file(override_path);
    }
    if !options.probe_default_keypool {
        return None;
    }
    if let Some(env_path) = std::env::var_os("LLMOS_KEYPOOL_CONFIG").filter(|v| !v.is_empty()) {
        let candidate = PathBuf::from(env_path);
        if let Some(summary) = probe_keypool_config_file(&candidate) {
            return Some(summary);
        }
    }
    let home = user_home_dir()?;
    probe_keypool_config_file(&home.join("glm-keypool").join("config.json"))
}

fn extract_port_from_json(json: &str) -> Option<u16> {
    for needle in ["\"port\"", "\"listenPort\""] {
        let mut rest = json;
        while let Some(idx) = rest.find(needle) {
            let after_key = &rest[idx + needle.len()..];
            let trimmed = after_key.trim_start();
            if let Some(after_colon) = trimmed.strip_prefix(':') {
                let value_start = after_colon.trim_start();
                let digits: String = value_start
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                if !digits.is_empty()
                    && let Ok(port) = digits.parse::<u16>()
                    && port > 0
                {
                    return Some(port);
                }
            }
            rest = after_key;
        }
    }
    None
}

fn load_or_generate_operator_key(key_file: &Path) -> Result<([u8; 32], KeyOrigin), UpError> {
    if key_file.exists() {
        let seed = read_existing_lowercase_hex_key(key_file)?;
        return Ok((seed, KeyOrigin::ReusedExisting));
    }
    let seed = generate_dev_trial_seed()?;
    let encoded = encode_hex(&seed);
    write_new_operator_key_file(key_file, &encoded)?;
    Ok((seed, KeyOrigin::GeneratedDevTrial))
}

fn read_existing_lowercase_hex_key(key_file: &Path) -> Result<[u8; 32], UpError> {
    check_unix_key_mode(key_file)?;
    let content = fs::read_to_string(key_file).map_err(UpError::Io)?;
    parse_lowercase_hex32(content.trim())
}

#[cfg(unix)]
fn check_unix_key_mode(key_file: &Path) -> Result<(), UpError> {
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::metadata(key_file)
        .map_err(UpError::Io)?
        .permissions()
        .mode()
        & 0o777;
    if mode != 0o600 {
        return Err(UpError::InsecureKeyMode(mode));
    }
    Ok(())
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn check_unix_key_mode(_key_file: &Path) -> Result<(), UpError> {
    Ok(())
}

#[cfg(unix)]
fn write_new_operator_key_file(key_file: &Path, hex_seed: &str) -> Result<(), UpError> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(key_file)
        .map_err(UpError::Io)?;
    file.write_all(hex_seed.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .map_err(UpError::Io)
}

#[cfg(not(unix))]
fn write_new_operator_key_file(key_file: &Path, hex_seed: &str) -> Result<(), UpError> {
    use std::io::Write as _;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(key_file)
        .map_err(UpError::Io)?;
    file.write_all(hex_seed.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .map_err(UpError::Io)
}

/// Generates a 32-byte local developer trial seed on Unix from `/dev/urandom`.
#[cfg(unix)]
fn generate_dev_trial_seed() -> Result<[u8; 32], UpError> {
    use std::io::Read as _;

    let mut seed = [0_u8; 32];
    let mut urandom = fs::File::open("/dev/urandom").map_err(UpError::Io)?;
    urandom.read_exact(&mut seed).map_err(UpError::Io)?;
    Ok(seed)
}

/// Generates a 32-byte local developer trial seed on Windows with zero `unsafe_code`.
///
/// Each `RandomState::new()` draws fresh OS randomness via Rust `std`'s internal
/// `BCryptGenRandom` seed; we sample 8 independent `RandomState` instances and mix
/// them with high-resolution `SystemTime` and `process::id()` through `SHA-256`.
/// Honest trust boundary: this key is strictly for local developer trial sessions
/// (`dev-trial-only`), never for production key custody.
#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn generate_dev_trial_seed() -> Result<[u8; 32], UpError> {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher as _, Hasher as _};

    let mut hasher = Sha256::new();
    hasher.update(b"llmos/nlos-up/windows-dev-trial-key-seed/v1");
    hasher.update(std::process::id().to_le_bytes());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0_u128, |elapsed| elapsed.as_nanos());
    hasher.update(nanos.to_le_bytes());
    for round in 0_u64..8 {
        let mut state_hasher = RandomState::new().build_hasher();
        state_hasher.write_u64(round);
        hasher.update(state_hasher.finish().to_le_bytes());
    }
    Ok(hasher.finalize().into())
}

fn bootstrap_operator_principal(
    root: &Path,
    public_key: [u8; 32],
) -> Result<(PrincipalId, bool), UpError> {
    let identity = IdentityAuthority::open(root.join("identity"))?;
    let decision = identity.bootstrap_principal(BootstrapPrincipalRequest {
        principal_profile_digest: digest32(
            b"llmos/system-control-daemon/principal-profile/v1",
            &public_key,
        ),
        control_domain_policy_digest: digest32(
            b"llmos/system-control-daemon/control-domain-policy/v1",
            &public_key,
        ),
        public_key,
        key_purpose: KeyPurpose::SemanticSigning,
        key_valid_from_ms: 0,
        key_valid_until_ms: BOOTSTRAP_KEY_VALID_UNTIL_MS,
        idempotency_key: IdempotencyKey::from_bytes(digest16(
            b"llmos/system-control-daemon/bootstrap-idempotency/v1",
            &public_key,
        )),
        created_at_ms: 0,
    })?;
    drop(identity);
    match decision {
        BootstrapDecision::Created(binding) => Ok((binding.principal_id, false)),
        BootstrapDecision::Replayed(binding) => Ok((binding.principal_id, true)),
    }
}

fn seed_trial_resource_budget(
    resource_root: &Path,
) -> Result<(AccountRecord, LlmTokenBudgetRecord, bool), UpError> {
    let resource = ResourceAuthority::open(resource_root)?;
    let account = resource.create_account(CreateAccountRequest {
        initial_credit: DEFAULT_TRIAL_INITIAL_CREDIT,
        idempotency_key: IdempotencyKey::from_bytes(digest16(
            TRIAL_ACCOUNT_KEY_DOMAIN,
            b"default-trial-account",
        )),
        created_at_ms: 0,
    })?;
    let budget_decision = resource.create_llm_token_budget(CreateLlmTokenBudgetRequest {
        account_id: account.account_id,
        limits: LlmTokenLimits {
            max_prompt_tokens: DEFAULT_TRIAL_MAX_PROMPT_TOKENS,
            max_completion_tokens: DEFAULT_TRIAL_MAX_COMPLETION_TOKENS,
            max_total_tokens: DEFAULT_TRIAL_MAX_TOTAL_TOKENS,
            max_api_calls: DEFAULT_TRIAL_MAX_API_CALLS,
            max_cost_microcredits: DEFAULT_TRIAL_MAX_COST_MICROCREDITS,
        },
        idempotency_key: IdempotencyKey::from_bytes(digest16(
            TRIAL_BUDGET_KEY_DOMAIN,
            account.account_id.as_bytes(),
        )),
        created_at_ms: 0,
    })?;
    // Explicitly close `ResourceAuthority` SQLite connection before daemon assembly.
    drop(resource);
    let (budget, replayed) = match budget_decision {
        LlmTokenBudgetDecision::Created(record) => (record, false),
        LlmTokenBudgetDecision::Replayed(record) => (record, true),
    };
    Ok((account, budget, replayed))
}

fn resolve_sample_package_dir(explicit_dir: Option<&Path>) -> Result<Option<PathBuf>, UpError> {
    if let Some(dir) = explicit_dir {
        if !dir.is_dir() {
            return Err(UpError::InvalidArgument(format!(
                "sample package directory does not exist: {}",
                dir.display()
            )));
        }
        Ok(Some(dir.to_path_buf()))
    } else {
        Ok(detect_repo_sample_package_dir())
    }
}

fn seed_sample_app_if_present(
    root: &Path,
    signing_key: &SigningKey,
    signer_principal: PrincipalId,
    explicit_dir: Option<&Path>,
) -> Result<SampleAppSeedStatus, UpError> {
    let Some(package_dir) = resolve_sample_package_dir(explicit_dir)? else {
        return Ok(SampleAppSeedStatus::Skipped);
    };

    let package_id = read_manifest_package_id(&package_dir.join("package.manifest"))?;
    let package_id_hex = encode_hex(package_id.as_bytes());

    let application = ApplicationAuthority::open(root.join("application"))?;
    if let Some(existing) = application.inspect_application(package_id)? {
        let application_id_hex = encode_hex(existing.application_id.as_bytes());
        drop(application);
        return Ok(SampleAppSeedStatus::AlreadyInstalled {
            package_id_hex,
            application_id_hex,
        });
    }

    let app_bin = fs::read(package_dir.join("app.bin"))?;
    let config_bin = fs::read(package_dir.join("assets").join("config.bin"))
        .unwrap_or_else(|_| b"nlos-up-sample-config".to_vec());

    let identity = IdentityAuthority::open(root.join("identity"))?;
    let artifacts = ArtifactStore::open(root.join("artifacts"))?;

    let driver_entry = materialize_sample_entry(
        &artifacts,
        "sample-driver",
        PackageEntryRole::Executable,
        &app_bin,
        1,
    )?;
    let config_entry = materialize_sample_entry(
        &artifacts,
        "sample-config",
        PackageEntryRole::Data,
        &config_bin,
        2,
    )?;

    let manifest = PackageManifest {
        package_id,
        version: 1,
        entries: vec![driver_entry, config_entry],
    };
    let signed = SignedPackage {
        signature: signing_key
            .sign(&package_manifest_message(&manifest))
            .to_bytes(),
        signer: signer_principal,
        manifest,
    };
    let verification = artifacts
        .verify_package(
            &identity,
            VerifyPackageRequest {
                signed: &signed,
                idempotency_key: IdempotencyKey::from_bytes(digest16(
                    SAMPLE_APP_KEY_DOMAIN,
                    b"verify-sample-package",
                )),
                verified_at_ms: 1_000,
            },
        )?
        .receipt()
        .clone();
    let receipt = match application.install_application(
        &artifacts,
        InstallApplicationRequest {
            package_verification_receipt_id: verification.receipt_id,
            idempotency_key: IdempotencyKey::from_bytes(digest16(
                SAMPLE_APP_KEY_DOMAIN,
                b"install-sample-package",
            )),
            installed_at_ms: 1_000,
        },
    )? {
        InstallDecision::Installed(installed) | InstallDecision::Replayed(installed) => installed,
    };

    let application_id_hex = encode_hex(receipt.application_id.as_bytes());
    // Explicitly close all SQLite connections before returning so daemon assembly never contends.
    drop(application);
    drop(artifacts);
    drop(identity);

    Ok(SampleAppSeedStatus::Installed {
        package_id_hex,
        application_id_hex,
    })
}

fn materialize_sample_entry(
    artifacts: &ArtifactStore,
    name: &str,
    role: PackageEntryRole,
    bytes: &[u8],
    slot: u8,
) -> Result<PackageManifestEntry, UpError> {
    let artifact_id = ArtifactId::from_bytes(digest16(SAMPLE_APP_KEY_DOMAIN, name.as_bytes()));
    artifacts.create_artifact(CreateArtifactSpec {
        artifact_id,
        idempotency_key: IdempotencyKey::from_bytes(digest16(SAMPLE_APP_KEY_DOMAIN, &[b'c', slot])),
        content_type: "application/octet-stream".to_owned(),
        application_id: None,
        owner: None,
        created_at_ms: 1_000,
    })?;
    match artifacts.put_revision(PutRevisionRequest {
        artifact_id,
        expected_head_revision: 0,
        bytes,
        created_at_ms: 1_000,
        provenance: provenance_triple(slot),
    }) {
        Ok(_) | Err(ArtifactError::HeadConflict { current: 1, .. }) => {}
        Err(error) => return Err(UpError::from(error)),
    }
    Ok(PackageManifestEntry {
        name: name.to_owned(),
        artifact_id,
        digest: ContentDigest::of_bytes(bytes),
        role,
    })
}

fn read_manifest_package_id(manifest_path: &Path) -> Result<PackageId, UpError> {
    let Ok(text) = fs::read_to_string(manifest_path) else {
        return Ok(PackageId::from_bytes(DEFAULT_SAMPLE_PACKAGE_ID));
    };
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=')
            && key.trim() == "package-id"
        {
            return parse_hex16(value.trim(), "sample package.manifest package-id")
                .map(PackageId::from_bytes);
        }
    }
    Ok(PackageId::from_bytes(DEFAULT_SAMPLE_PACKAGE_ID))
}

fn detect_repo_sample_package_dir() -> Option<PathBuf> {
    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join("examples").join("sample-app").join("package");
        if candidate.join("app.bin").is_file() {
            return Some(candidate);
        }
    }
    let workspace_candidate =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/sample-app/package");
    workspace_candidate
        .join("app.bin")
        .is_file()
        .then_some(workspace_candidate)
}

fn detect_sibling_cli_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let bin_name = if cfg!(windows) {
        "system-control-cli.exe"
    } else {
        "system-control-cli"
    };
    let candidate = dir.join(bin_name);
    candidate.is_file().then_some(candidate)
}

fn write_marker_file(path: &Path, content: &str) -> Result<(), UpError> {
    fs::write(path, format!("{content}\n")).map_err(UpError::Io)
}

fn read_hex16_marker(path: &Path, label: &str) -> Result<String, UpError> {
    let raw = fs::read_to_string(path).map_err(UpError::Io)?;
    let trimmed = raw.trim();
    let bytes = parse_hex16(trimmed, label)?;
    Ok(encode_hex(&bytes))
}

fn parse_lowercase_hex32(raw: &str) -> Result<[u8; 32], UpError> {
    if raw.len() != 64
        || !raw
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(UpError::InvalidKeyFile(
            "expected exactly 64 lowercase hex characters of Ed25519 seed".to_owned(),
        ));
    }
    let mut out = [0_u8; 32];
    for (idx, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&raw[2 * idx..2 * idx + 2], 16).map_err(|error| {
            UpError::InvalidKeyFile(format!("failed to decode hex byte {idx}: {error}"))
        })?;
    }
    Ok(out)
}

fn parse_hex16(raw: &str, label: &str) -> Result<[u8; 16], UpError> {
    if raw.len() != 32 || !raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(UpError::InvalidArgument(format!(
            "{label} must be 32 hex characters"
        )));
    }
    let mut out = [0_u8; 16];
    for (idx, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&raw[2 * idx..2 * idx + 2], 16)
            .map_err(|error| UpError::InvalidArgument(format!("{label}: {error}")))?;
    }
    Ok(out)
}

/// Encodes raw bytes as lowercase hexadecimal.
#[must_use]
pub fn encode_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn digest32(domain: &[u8], payload: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(payload);
    hasher.finalize().into()
}

fn digest16(domain: &[u8], payload: &[u8]) -> [u8; 16] {
    let full = digest32(domain, payload);
    let mut short = [0_u8; 16];
    short.copy_from_slice(&full[..16]);
    short
}

fn escape_powershell_double_quoted(value: &str) -> String {
    value
        .replace('`', "``")
        .replace('"', "`\"")
        .replace('$', "`$")
}

fn escape_posix_single_quoted(value: &str) -> String {
    value.replace('\'', "'\\''")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::Path;

    use super::{
        CRATE_NAME, DEFAULT_KEYPOOL_PORT, ShellKind, default_trial_root_from_vars,
        extract_port_from_json, parse_lowercase_hex32,
    };

    #[test]
    fn scaffold_crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, env!("CARGO_PKG_NAME"));
    }

    #[test]
    fn default_root_precedence_prefers_llmos_home_then_user_home_then_temp() {
        let temp = Path::new("/tmp/fallback");
        let home = Path::new("/home/tester");
        assert_eq!(
            default_trial_root_from_vars(Some(OsStr::new("/custom/llmos")), Some(home), temp),
            Path::new("/custom/llmos").join("trial-state")
        );
        assert_eq!(
            default_trial_root_from_vars(None, Some(home), temp),
            home.join(".llmos").join("trial-state")
        );
        assert_eq!(
            default_trial_root_from_vars(None, None, temp),
            temp.join("llmos-trial-state")
        );
    }

    #[test]
    fn lowercase_hex32_rejects_uppercase_and_wrong_length() {
        let valid = "0123456789abcdef".repeat(4);
        assert!(parse_lowercase_hex32(&valid).is_ok());
        let upper = "0123456789ABCDEF".repeat(4);
        assert!(parse_lowercase_hex32(&upper).is_err());
        assert!(parse_lowercase_hex32("abcd").is_err());
    }

    #[test]
    fn keypool_port_parser_extracts_port_without_touching_other_fields() {
        let sample = r#"{"host":"127.0.0.1","port": 18317,"egressKey":"unused"}"#;
        assert_eq!(extract_port_from_json(sample), Some(18317));
        assert_eq!(
            extract_port_from_json("{}").unwrap_or(DEFAULT_KEYPOOL_PORT),
            8317
        );
    }

    #[test]
    fn shell_kind_parses_powershell_and_posix() {
        assert_eq!(
            ShellKind::parse("powershell").ok(),
            Some(ShellKind::PowerShell)
        );
        assert_eq!(ShellKind::parse("posix").ok(), Some(ShellKind::Posix));
        assert!(ShellKind::parse("cmd-unknown").is_err());
    }
}
