//! Typed fail-closed error hierarchy for `nlos-wasm-sandbox` (`ADR-0015`;
//! `ADR-0027` section 2.3; `[TCB-BYPASS-001]`, `[TCB-EGRESS-001]`,
//! `[TCB-DOWNGRADE-001]`).
//!
//! Every failure mode that the sandbox can detect is reported as a distinct
//! variant so that the escape-adversarial test matrix can assert on concrete
//! error identities instead of bare `is_err()` results. Sanitized detail
//! strings exist solely for the child-process report protocol; they are
//! redacted to report-line-safe tokens by [`sanitize_detail`].

use std::error::Error;
use std::fmt;

/// Structural violations of the sandbox profile invariants
/// ([`crate::SandboxProfile`]). All validation is fail-closed: an invalid
/// profile never reaches the engine.
#[derive(Debug)]
pub enum ProfileInvalid {
    /// `fuel` was zero; the deterministic CPU quota must be positive.
    FuelZero,
    /// `max_memory_pages` was outside the enforced `1..=65536` window.
    MemoryPagesOutOfRange {
        /// The rejected page cap value.
        pages: u64,
    },
    /// A preopen guest directory did not start with `/` or was empty.
    PreopenGuestDirNotAbsolute {
        /// The rejected guest-facing directory name.
        guest_dir: String,
    },
    /// A preopen guest directory contained `.`, `..`, a backslash, a NUL
    /// byte, or a trailing slash.
    PreopenGuestDirUnsafeComponent {
        /// The rejected guest-facing directory name.
        guest_dir: String,
    },
    /// The preopen host root does not exist, is not a directory, or cannot
    /// be canonicalized.
    PreopenHostRootInvalid {
        /// The rejected host-side path.
        host_root: std::path::PathBuf,
    },
    /// A guest argument or the entrypoint name contained a NUL byte or was
    /// not valid UTF-8.
    EmbeddedNul {
        /// Which structurally invalid field was rejected.
        field: &'static str,
    },
    /// An epoch deadline was configured with a zero ticker interval, which
    /// could never fire; rejected rather than silently degrading the
    /// wall-clock guarantee.
    EpochIntervalMissing,
    /// The entrypoint name was empty.
    EntrypointEmpty,
}

impl fmt::Display for ProfileInvalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FuelZero => write!(f, "sandbox profile fuel quota must be >= 1"),
            Self::MemoryPagesOutOfRange { pages } => write!(
                f,
                "sandbox profile memory cap {pages} pages outside 1..=65536"
            ),
            Self::PreopenGuestDirNotAbsolute { guest_dir } => write!(
                f,
                "preopen guest dir {guest_dir} must be non-empty and start with '/'"
            ),
            Self::PreopenGuestDirUnsafeComponent { guest_dir } => write!(
                f,
                "preopen guest dir {guest_dir} must not contain '.', '..', a backslash, NUL, or a trailing slash"
            ),
            Self::PreopenHostRootInvalid { host_root } => write!(
                f,
                "preopen host root {} must be an existing canonicalizable directory",
                host_root.display()
            ),
            Self::EpochIntervalMissing => {
                write!(f, "epoch deadline requires a positive ticker interval (ms)")
            }
            Self::EmbeddedNul { field } => {
                write!(f, "{field} must be valid UTF-8 without NUL bytes")
            }
            Self::EntrypointEmpty => write!(f, "entrypoint name must be non-empty"),
        }
    }
}

impl Error for ProfileInvalid {}

/// Classified trap kinds observed when untrusted guest execution is
/// terminated by the Wasmtime engine (`[DRV-LIFE-001]` lifecycle terminal
/// states). Each kind maps to a stable report token used by the
/// child-process protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrapKind {
    /// Deterministic CPU fuel quota exhausted (`Trap::OutOfFuel`).
    OutOfFuel,
    /// Epoch interruption deadline reached (`Trap::Interrupt`).
    EpochInterrupt,
    /// Guest executed the `unreachable` instruction.
    UnreachableCodeReached,
    /// Guest invoked `proc_exit` (cooperative termination, not a fault).
    HostAbort,
    /// Any other trap; `detail` carries a sanitized engine message.
    Other {
        /// Report-line-safe sanitized trap detail.
        detail: String,
    },
}

impl TrapKind {
    /// Returns the stable lowercase token used in report lines
    /// (`out-of-fuel`, `epoch-interrupt`, ...).
    #[must_use]
    pub fn report_token(&self) -> String {
        match self {
            Self::OutOfFuel => "out-of-fuel".to_string(),
            Self::EpochInterrupt => "epoch-interrupt".to_string(),
            Self::UnreachableCodeReached => "unreachable".to_string(),
            Self::HostAbort => "host-abort".to_string(),
            Self::Other { detail } => format!("other:{detail}"),
        }
    }
}

impl fmt::Display for TrapKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.report_token())
    }
}

/// Fail-closed sandbox error hierarchy. Host-side orchestration failures,
/// guest-side engine rejections, and profile violations are disjoint
/// variants so callers and tests can assert on exact identities.
#[derive(Debug)]
pub enum SandboxError {
    /// The sandbox profile violated structural invariants.
    Profile(ProfileInvalid),
    /// The untrusted module bytes were rejected at compile time (invalid
    /// WAT/wasm). `detail` is sanitized.
    GuestModuleRejected {
        /// Report-line-safe sanitized compile error detail.
        detail: String,
    },
    /// Instantiation was rejected — includes unknown imports (for example
    /// socket imports absent from the allowlist surface) and linear memory
    /// minimums exceeding the page cap. `detail` is sanitized.
    GuestInstantiationRejected {
        /// Report-line-safe sanitized instantiation error detail.
        detail: String,
    },
    /// The requested entrypoint export does not exist on the guest module.
    EntrypointMissing {
        /// The missing export name.
        name: String,
    },
    /// The entrypoint exists but does not have the required `() -> i32`
    /// signature.
    EntrypointSignatureInvalid {
        /// The offending export name.
        name: String,
    },
    /// An engine-level failure other than a classified trap.
    GuestEngineFailure {
        /// Report-line-safe sanitized engine error detail.
        detail: String,
    },
    /// Host process orchestration I/O failure (spawn, wait, pipes).
    HostIo(std::io::Error),
    /// The host binary exited without emitting a parsable report line.
    HostReportMissing {
        /// Tail of captured child stderr for diagnosis.
        stderr_tail: String,
    },
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Profile(e) => write!(f, "invalid sandbox profile: {e}"),
            Self::GuestModuleRejected { detail } => {
                write!(f, "untrusted module rejected at compile: {detail}")
            }
            Self::GuestInstantiationRejected { detail } => {
                write!(f, "untrusted module rejected at instantiation: {detail}")
            }
            Self::EntrypointMissing { name } => {
                write!(f, "guest entrypoint export {name:?} is missing")
            }
            Self::EntrypointSignatureInvalid { name } => {
                write!(
                    f,
                    "guest entrypoint export {name:?} must have signature () -> i32"
                )
            }
            Self::GuestEngineFailure { detail } => {
                write!(f, "wasmtime engine failure: {detail}")
            }
            Self::HostIo(e) => write!(f, "host process orchestration I/O failure: {e}"),
            Self::HostReportMissing { stderr_tail } => {
                write!(
                    f,
                    "host binary produced no parsable report (stderr tail: {stderr_tail})"
                )
            }
        }
    }
}

impl Error for SandboxError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Profile(e) => Some(e),
            Self::HostIo(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for SandboxError {
    fn from(e: std::io::Error) -> Self {
        Self::HostIo(e)
    }
}

/// Outcome token of a host-binary report line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOutcome {
    /// Guest entrypoint returned normally.
    Completed,
    /// Guest invoked `proc_exit` with the reported code.
    GuestExit,
    /// Guest execution was terminated by a classified trap.
    Trap,
    /// The host binary failed before or during guest execution.
    HostError,
    /// `--self-audit-env` environment introspection mode.
    AuditEnv,
}

impl HostOutcome {
    /// Parses the report-line token for this outcome.
    #[must_use]
    pub fn parse_token(token: &str) -> Option<Self> {
        match token {
            "completed" => Some(Self::Completed),
            "guest-exit" => Some(Self::GuestExit),
            "trap" => Some(Self::Trap),
            "host-error" => Some(Self::HostError),
            "audit-env" => Some(Self::AuditEnv),
            _ => None,
        }
    }
}

/// Structured child-process report parsed from the `NLOS-HOST-R1` line the
/// dedicated host binary writes to its stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostReport {
    /// Outcome classification of the guest run.
    pub outcome: HostOutcome,
    /// Outcome-specific numeric code (guest return value, `proc_exit` code,
    /// observed errno, or environment variable count in audit mode).
    pub code: i64,
    /// Sanitized detail token (trap kind, host error variant, ...).
    pub detail: String,
    /// Number of bytes the guest wrote to its stdout descriptor.
    pub guest_stdout_len: u64,
}

/// Redacts an arbitrary engine or I/O error message into a token that is
/// safe for a single-line report: printable ASCII letters, digits,
/// `-`, `_`, `.` and `:` are kept, every other byte becomes `-`, and the
/// result is capped to keep report lines bounded.
#[must_use]
pub fn sanitize_detail(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(160));
    for ch in raw.chars() {
        if out.len() >= 160 {
            break;
        }
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':') {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trap_kind_tokens_are_stable() {
        assert_eq!(TrapKind::OutOfFuel.report_token(), "out-of-fuel");
        assert_eq!(TrapKind::EpochInterrupt.report_token(), "epoch-interrupt");
        assert_eq!(
            TrapKind::UnreachableCodeReached.report_token(),
            "unreachable"
        );
        assert_eq!(TrapKind::HostAbort.report_token(), "host-abort");
        assert_eq!(
            TrapKind::Other {
                detail: "wasm-trap:-integer-overflow".to_string()
            }
            .report_token(),
            "other:wasm-trap:-integer-overflow"
        );
    }

    #[test]
    fn sanitize_detail_keeps_report_line_single_line_and_bounded() {
        let raw = "unknown import: `wasi_snapshot_preview1::sock_open` has not been defined\nsecond line\twith tab";
        let sanitized = sanitize_detail(raw);
        assert!(!sanitized.contains('\n'), "newline must be redacted");
        assert!(!sanitized.contains('\t'), "tab must be redacted");
        assert!(sanitized.contains("sock_open"), "import name must survive");
        assert!(sanitized.len() <= 160);
    }

    #[test]
    fn host_outcome_tokens_round_trip() {
        for outcome in [
            HostOutcome::Completed,
            HostOutcome::GuestExit,
            HostOutcome::Trap,
            HostOutcome::HostError,
            HostOutcome::AuditEnv,
        ] {
            let token = match outcome {
                HostOutcome::Completed => "completed",
                HostOutcome::GuestExit => "guest-exit",
                HostOutcome::Trap => "trap",
                HostOutcome::HostError => "host-error",
                HostOutcome::AuditEnv => "audit-env",
            };
            assert_eq!(HostOutcome::parse_token(token), Some(outcome));
        }
        assert_eq!(HostOutcome::parse_token("bogus"), None);
    }
}
