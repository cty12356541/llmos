//! The natural-language control surface of the slice (W30-D): one
//! restricted-grammar sentence → [`nlos_system_control::parse_nl_command`]
//! (the landed NL compiler, §1.3 / `[NLOS-NL-001/002]`) → the same
//! `ControlCommand` every other surface dispatches →
//! [`nlos_system_control::control::dispatch_in_process`] → the one
//! `RecoverySystemControl` handler (envelope compilation, authorization
//! seam, §25.3 idempotency binding) → the landed pluggable seams wired to
//! this runtime's real authorities:
//!
//! - `inspect process <32-hex>` → `ProcessAuthorityInspector` over this
//!   runtime's process authority (readback-validated binding facts);
//! - `kill operation <32-hex> expecting <generation>` →
//!   `ProcessAuthorityKillExecutor` over this runtime's process authority
//!   + supervisor pid registry + the caller's platform kill adapter — the
//!     same durable platform-kill path the teardown lane drives.
//!
//! There is no bypass: the sentence never reaches an authority directly,
//! and this crate adds no command semantics — the grammar, the command
//! enum, the envelope, and the handler all belong to `nlos-system-control`
//! (read-only consumption). Repeating a mutation sentence replays the
//! identical durable receipt: the NL kill's command identity derives from
//! the target id, so one process has exactly one kill identity.
//!
//! The two fixtures below are slice-owned wiring, not policy claims: the
//! allow-all authorizer stands where a host injects its real policy (the
//! parity contract is that NL crosses the *same* `authorize_submit` seam
//! as the CLI and IPC surfaces), and the health source reports the
//! recovery worker state the demo operates under.

use nlos_commit_coordinator::{RecoveryWorkerHealth, RecoveryWorkerState};
use nlos_process::{PlatformKillAdapter, SupervisorPidRegistry};
use nlos_schema::sabi::v1::{GetSystemControlRequest, SabiRequestContext};
use nlos_system_control::control::{ControlCommand, ControlReceipt, dispatch_in_process};
use nlos_system_control::nl::parse_nl_command;
use nlos_system_control::process_inspector::ProcessAuthorityInspector;
use nlos_system_control::process_kill_executor::ProcessAuthorityKillExecutor;
use nlos_system_control::{RecoveryHealthSource, RecoverySystemControl, SystemControlAuthorizer};
use sha2::{Digest, Sha256};

use crate::error::{SliceKError, SliceKResult};
use crate::runtime::SliceKRuntime;

/// Domain separator of the NL dispatcher's per-command clock keys: the
/// same sentence replays the same wall reading, so a repeated mutation
/// reconstructs a byte-identical executor request.
const NL_WALL_KEY_DOMAIN: &[u8] = b"nlos/slice-k/nl-wall/v1";

/// Monotonic fixture of the in-process dispatch (the envelope contract
/// only needs `deadline ≤ now`; the slice runs in one process).
const NL_MONOTONIC_NOW_NS: u64 = 10;

/// Slice fixture: the recovery worker state reported to the control
/// handler's health reads (`Running`, the demo's operating state).
pub struct RunningRecoveryHealth {
    health: RecoveryWorkerHealth,
}

impl RunningRecoveryHealth {
    #[must_use]
    pub fn new() -> Self {
        Self {
            health: RecoveryWorkerHealth {
                state: RecoveryWorkerState::Running,
                ..RecoveryWorkerHealth::default()
            },
        }
    }
}

impl Default for RunningRecoveryHealth {
    fn default() -> Self {
        Self::new()
    }
}

impl RecoveryHealthSource for RunningRecoveryHealth {
    fn recovery_health(&self) -> RecoveryWorkerHealth {
        self.health.clone()
    }
}

/// Slice fixture authorizer: allow-all, standing exactly where a host
/// wires its real policy — NL, CLI, and IPC all cross this same seam.
#[derive(Clone, Copy, Debug, Default)]
pub struct SliceKControlPolicy;

impl SystemControlAuthorizer for SliceKControlPolicy {
    fn authorize_get(
        &self,
        _context: &SabiRequestContext,
        _request: &GetSystemControlRequest,
    ) -> Result<(), &'static str> {
        Ok(())
    }

    fn authorize_submit(
        &self,
        _context: &SabiRequestContext,
        _command: &nlos_schema::sabi::v1::ControlCommand,
    ) -> Result<(), &'static str> {
        Ok(())
    }
}

/// Compiles and dispatches one natural-language control sentence through
/// the real control chain over this runtime's authorities. The wall
/// reading handed to the handler comes from this runtime's clock under a
/// key derived from the parsed command identity, so repeating the same
/// sentence replays the same durable effects (an NL kill re-derives the
/// identical platform-kill receipt, never a second signal).
///
/// # Errors
///
/// Returns [`SliceKError::Control`] for an out-of-grammar sentence or a
/// dispatch-contract defect. Handler rejections (unknown process, CAS
/// mismatch, absent supervisor mapping) are *not* errors here — they
/// surface as the typed `SabiFailure` inside the returned
/// [`ControlReceipt::outcome`], exactly like the CLI surface.
pub fn dispatch_nl_command(
    runtime: &SliceKRuntime,
    supervisor: &SupervisorPidRegistry,
    kill_adapter: &(impl PlatformKillAdapter + Sync),
    sentence: &str,
) -> SliceKResult<ControlReceipt> {
    let command: ControlCommand = parse_nl_command(sentence).map_err(SliceKError::Control)?;
    let now_wall_ms = runtime.wall_now_i64(nl_wall_key(command.control_command_id()))?;
    let health = RunningRecoveryHealth::new();
    let executor = ProcessAuthorityKillExecutor::new(&runtime.process, supervisor, kill_adapter);
    let control = RecoverySystemControl::new(&runtime.tasks, &health, &SliceKControlPolicy)
        .with_operation_executor(&executor);
    dispatch_in_process(
        &control,
        &command,
        NL_MONOTONIC_NOW_NS,
        now_wall_ms,
        Some(&ProcessAuthorityInspector::new(&runtime.process)),
        None,
        None,
    )
    .map_err(SliceKError::Control)
}

/// The per-command wall-clock key: SHA-16 over the NL domain and the
/// command identity.
fn nl_wall_key(control_command_id: [u8; 16]) -> nlos_types::IdempotencyKey {
    let digest = Sha256::new()
        .chain_update(NL_WALL_KEY_DOMAIN)
        .chain_update(control_command_id)
        .finalize();
    let mut key = [0_u8; 16];
    key.copy_from_slice(&digest[..16]);
    nlos_types::IdempotencyKey::from_bytes(key)
}
