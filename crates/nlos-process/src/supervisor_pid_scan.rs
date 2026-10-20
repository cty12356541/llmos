//! Supervisor pid discovery / scan entry (W47-L1, the `C-APP-CONTROL` #11
//! pid-discovery half).
//!
//! [`ProcessSupervisor::scan_pids`] is the scanning entry the supervisor
//! face lacked: it takes the process authority's persistent bindings as its
//! source — `ProcessAuthority::list_active_process_bindings` narrows the
//! durable set to one [`SupervisorPidScanTarget`] per non-terminal binding —
//! resolves each target's current OS pid, and reports a pure
//! classification with zero durable side effects (no registry write, no
//! authority write, no OS signal):
//!
//! - [`SupervisorPidFinding::Fresh`] — the binding has a verified-live OS
//!   pid backing it: the registry's recorded pid at exactly the target
//!   generation (the recorded pid is preferred, and only then verified), or
//!   a resolver-supplied candidate pid that the probe confirms live;
//! - [`SupervisorPidFinding::Stale`] — a pid is recorded (or resolved) but
//!   cannot back the binding: registry generation drift, a dead recorded
//!   pid, or a dead resolved candidate
//!   ([`SupervisorPidStaleReason`] carries which);
//! - [`SupervisorPidFinding::Missing`] — no pid is known for the binding
//!   and the resolver offers none.
//!
//! Verification and OS-side resolution are injected through the
//! [`SupervisorPidProbe`] hook, so the classification stays testable and
//! host-independent. [`HostSupervisorPidProbe`] is the real host probe —
//! a `kill(pid, 0)` existence check on Unix, a `tasklist` filter query on
//! Windows, typed fail-closed elsewhere — and ships no OS-side resolver
//! (the trait default returns none): scanning the host process table for
//! never-registered pids is a later slice, not an invented capability.
//! A same-generation dead pid deliberately skips the resolver: the
//! registry owns exactly one OS pid per generation, so a replacement pid
//! for the same generation is the caller's explicit
//! unregister-plus-new-generation path, never a silent rebind.
//!
//! Consumption is explicit opt-in: [`ProcessSupervisor::adopt_fresh_findings`]
//! registers each fresh `(process, generation, os pid)` through the normal
//! [`crate::SupervisorPidRegistry::register`] gates (exact replay, supersede on a
//! strictly newer generation, fail-closed same-generation pid rebind), so
//! the existing registration semantics are unchanged and adoption is
//! idempotent under retry. Stale and missing findings are never adopted:
//! they are the report a caller acts on with its own kill / restore /
//! unregister decisions.

use nlos_types::{Generation, ProcessId};

use crate::model::ProcessBindingRecord;
use crate::supervisor::{ProcessSupervisor, SupervisorError};
use crate::supervisor_pid::{
    RegisterSupervisorPidRequest, SupervisorPidDecision, SupervisorPidEntry,
    SupervisorPidRegistryError,
};

/// One durable binding a scan resolves: the authority-assigned Process
/// identity plus the head generation the pid must back. Build these from
/// `ProcessAuthority::list_active_process_bindings` (the persistent
/// non-terminal binding set).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupervisorPidScanTarget {
    pub process_id: ProcessId,
    pub process_generation: Generation,
}

impl From<&ProcessBindingRecord> for SupervisorPidScanTarget {
    fn from(record: &ProcessBindingRecord) -> Self {
        Self {
            process_id: record.process_id,
            process_generation: record.process_generation,
        }
    }
}

/// The liveness verdict of one probed OS pid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorPidLiveness {
    /// The OS pid currently names a live host process.
    Alive,
    /// The OS pid names no live host process (`ESRCH`-family).
    Dead,
}

/// Why a scanned binding's pid cannot back it
/// ([`SupervisorPidFinding::Stale`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorPidStaleReason {
    /// The registry maps the Process at a generation other than the
    /// target's authority head generation (the recorded pid belongs to a
    /// different incarnation fence), and the resolver offered no live
    /// replacement.
    GenerationDrift { recorded_generation: Generation },
    /// The recorded pid sits at the target generation but the probe found
    /// its host process dead.
    RecordedPidDead,
    /// The resolver supplied a candidate pid for the binding and the probe
    /// found that candidate dead.
    CandidatePidDead { candidate: u32 },
}

/// One resolved scan row: the target plus its fresh / stale / missing
/// classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorPidFinding {
    /// The binding has a verified-live OS pid backing it — the registry's
    /// recorded pid at the target generation, or a resolver-supplied
    /// candidate the probe confirmed live. The only finding
    /// [`ProcessSupervisor::adopt_fresh_findings`] consumes.
    Fresh {
        target: SupervisorPidScanTarget,
        os_pid: u32,
    },
    /// A pid is recorded (and/or resolved) but cannot back the binding;
    /// `recorded` carries the registry row when one exists and `reason`
    /// says which fence failed.
    Stale {
        target: SupervisorPidScanTarget,
        recorded: Option<SupervisorPidEntry>,
        reason: SupervisorPidStaleReason,
    },
    /// No pid is known for the binding: nothing is registered and the
    /// resolver offers no candidate.
    Missing { target: SupervisorPidScanTarget },
}

impl SupervisorPidFinding {
    /// The scan target this finding classifies.
    #[must_use]
    pub const fn target(&self) -> &SupervisorPidScanTarget {
        match self {
            Self::Fresh { target, .. } | Self::Stale { target, .. } | Self::Missing { target } => {
                target
            }
        }
    }
}

/// The pure result of one [`ProcessSupervisor::scan_pids`] pass: one
/// finding per target, in target order. Repeating the scan with the same
/// probe answers produces the equal report — the scan writes nothing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SupervisorPidScanReport {
    findings: Vec<SupervisorPidFinding>,
}

impl SupervisorPidScanReport {
    /// Every finding in target order.
    #[must_use]
    pub fn findings(&self) -> &[SupervisorPidFinding] {
        &self.findings
    }

    /// Whether the scan had no targets to resolve.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.findings.is_empty()
    }
}

/// The injection point for pid verification and OS-side resolution
/// ([`ProcessSupervisor::scan_pids`]).
///
/// Implementations must be side-effect bounded to the presented pid or
/// target: the scan treats the probe as a read of host state, so a scan
/// report stays reproducible (idempotent) exactly as long as the probe's
/// answers do not change.
pub trait SupervisorPidProbe {
    /// Verifies whether `os_pid` currently names a live host process.
    ///
    /// # Errors
    ///
    /// A probe failure (host syscall or subprocess failure) fails the whole
    /// scan closed with zero side effect: a classification built on an
    /// unverifiable pid would be a guess, not a discovery.
    fn os_pid_liveness(&mut self, os_pid: u32) -> Result<SupervisorPidLiveness, SupervisorError>;

    /// Resolves the current OS pid for a binding the registry cannot back
    /// (no mapping, or a mapping at another generation) from an OS-side or
    /// external source. The default offers nothing — the crate mints no
    /// host-process-table scanner in this slice.
    fn resolve_os_pid(&mut self, target: SupervisorPidScanTarget) -> Option<u32> {
        let _ = target;
        None
    }
}

/// The real host liveness probe, mirroring the supervisor's platform
/// matrix: a `kill(pid, 0)` existence check on Unix and a `tasklist`
/// filter query on Windows (both side-effect-free reads of host state,
/// no signal is delivered); hosts with no safe probe fail closed with
/// [`SupervisorError::UnsupportedOnPlatform`]. Ships the trait's default
/// (no-op) resolver.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HostSupervisorPidProbe;

#[cfg(unix)]
impl SupervisorPidProbe for HostSupervisorPidProbe {
    fn os_pid_liveness(&mut self, os_pid: u32) -> Result<SupervisorPidLiveness, SupervisorError> {
        use nix::errno::Errno;
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        // The null signal is the existence check: no signal is delivered,
        // error reporting still runs. `EPERM` means the pid exists but
        // belongs to another principal — the probe asks existence, not
        // permission to signal.
        match kill(Pid::from_raw(os_pid.cast_signed()), None) {
            Ok(()) | Err(Errno::EPERM) => Ok(SupervisorPidLiveness::Alive),
            Err(Errno::ESRCH) => Ok(SupervisorPidLiveness::Dead),
            Err(_) => Err(SupervisorError::LivenessProbe(
                "unix null-signal existence check failed for os pid",
            )),
        }
    }
}

#[cfg(windows)]
impl SupervisorPidProbe for HostSupervisorPidProbe {
    fn os_pid_liveness(&mut self, os_pid: u32) -> Result<SupervisorPidLiveness, SupervisorError> {
        // CSV rows quote every field, so an exact quoted-field compare
        // cannot confuse the pid with an image name or a memory count;
        // `tasklist` exits zero even when the filter matches nothing, so
        // the row scan — not the exit code — is the verdict.
        let filter = format!("PID eq {os_pid}");
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &filter, "/FO", "CSV", "/NH"])
            .output()
            .map_err(|_| SupervisorError::LivenessProbe("tasklist spawn failed for os pid"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let quoted_pid = format!("\"{os_pid}\"");
        let alive = stdout
            .lines()
            .any(|line| line.split(',').any(|field| field == quoted_pid));
        Ok(if alive {
            SupervisorPidLiveness::Alive
        } else {
            SupervisorPidLiveness::Dead
        })
    }
}

#[cfg(not(any(unix, windows)))]
impl SupervisorPidProbe for HostSupervisorPidProbe {
    fn os_pid_liveness(&mut self, _os_pid: u32) -> Result<SupervisorPidLiveness, SupervisorError> {
        Err(SupervisorError::UnsupportedOnPlatform {
            operation: "os pid liveness probe",
        })
    }
}

impl ProcessSupervisor {
    /// Scans `targets` (the authority's persistent non-terminal bindings)
    /// and classifies each one's current OS pid — the supervisor pid
    /// discovery entry point. Pure read plus report: no registry write, no
    /// authority write, no OS signal, so a rescan with unchanged probe
    /// answers returns the equal report (idempotent) and a failed probe
    /// leaves every side of the system untouched (fail-closed).
    ///
    /// Resolution order per target (the recorded pid is preferred, the
    /// resolver only fills gaps the registry cannot back):
    ///
    /// 1. a registry mapping at exactly the target generation → verify its
    ///    recorded pid through the probe; live is
    ///    [`SupervisorPidFinding::Fresh`], dead is
    ///    [`SupervisorPidFinding::Stale`] with
    ///    [`SupervisorPidStaleReason::RecordedPidDead`] (the resolver is
    ///    not consulted: one generation owns exactly one pid, so a
    ///    same-generation replacement is the caller's explicit
    ///    unregister-plus-new-generation path, never a silent rebind);
    /// 2. otherwise ask [`SupervisorPidProbe::resolve_os_pid`] for the
    ///    binding's current pid; a probe-live candidate is `Fresh`, a dead
    ///    candidate is `Stale` with
    ///    [`SupervisorPidStaleReason::CandidatePidDead`];
    /// 3. no mapping and no candidate → the drifted registry row (if any)
    ///    reports `Stale` with
    ///    [`SupervisorPidStaleReason::GenerationDrift`], and an unmapped
    ///    target reports [`SupervisorPidFinding::Missing`].
    ///
    /// # Errors
    ///
    /// Fails closed on a poisoned registry lock or any probe failure;
    /// either way no report is produced and nothing was written.
    pub fn scan_pids(
        &self,
        targets: &[SupervisorPidScanTarget],
        probe: &mut impl SupervisorPidProbe,
    ) -> Result<SupervisorPidScanReport, SupervisorError> {
        let mut findings = Vec::with_capacity(targets.len());
        for &target in targets {
            findings.push(self.scan_one_target(target, probe)?);
        }
        Ok(SupervisorPidScanReport { findings })
    }

    /// Opt-in consumption of a discovery report: registers each
    /// [`SupervisorPidFinding::Fresh`] row's `(process, generation, os
    /// pid)` into the supervisor registry through the unchanged
    /// [`crate::SupervisorPidRegistry::register`] gates — a fresh-from-record row
    /// replays idempotently (the original entry, original
    /// `registered_at_ms`, untouched), a fresh-discovered row for an
    /// unmapped process registers, and one behind a drifted row supersedes
    /// it to the authority's generation. `Stale` / `Missing` findings are
    /// skipped, nothing is unregistered, no OS signal is issued, and the
    /// authority is not consulted.
    ///
    /// Adoption is at-most-once per row and self-healing under retry: the
    /// decisions return in fresh-finding order, and a mid-adoption
    /// fail-closed rejection (for example a same-generation pid rebind
    /// against a mapping registered after the scan) surfaces with the rows
    /// before it already applied — retrying the same report replays those
    /// rows and re-attempts the rest.
    ///
    /// # Errors
    ///
    /// Fails closed on any registry rejection (stale generation, same
    /// generation pid rebind, poisoned lock) with the prefix of decisions
    /// already applied staying in place.
    pub fn adopt_fresh_findings(
        &self,
        report: &SupervisorPidScanReport,
        registered_at_ms: u64,
    ) -> Result<Vec<SupervisorPidDecision>, SupervisorError> {
        let mut decisions = Vec::new();
        for finding in report.findings() {
            let SupervisorPidFinding::Fresh { target, os_pid } = finding else {
                continue;
            };
            decisions.push(
                self.registry()
                    .register(RegisterSupervisorPidRequest {
                        process_id: target.process_id,
                        process_generation: target.process_generation,
                        os_pid: *os_pid,
                        registered_at_ms,
                    })
                    .map_err(SupervisorError::Registry)?,
            );
        }
        Ok(decisions)
    }

    fn scan_one_target(
        &self,
        target: SupervisorPidScanTarget,
        probe: &mut impl SupervisorPidProbe,
    ) -> Result<SupervisorPidFinding, SupervisorError> {
        match self.registry().lookup(target.process_id) {
            Ok(entry) => classify_recorded_target(target, entry, probe),
            Err(SupervisorPidRegistryError::ProcessNotRegistered(_)) => {
                classify_resolver_backed_target(target, None, probe)
            }
            Err(error) => Err(SupervisorError::Registry(error)),
        }
    }
}

/// Classifies a target the registry maps: the recorded pid is preferred
/// and verified in place when its generation matches the target; a
/// drifted row falls through to the resolver path carrying its context.
fn classify_recorded_target(
    target: SupervisorPidScanTarget,
    entry: SupervisorPidEntry,
    probe: &mut impl SupervisorPidProbe,
) -> Result<SupervisorPidFinding, SupervisorError> {
    if entry.process_generation != target.process_generation {
        return classify_resolver_backed_target(target, Some(entry), probe);
    }
    match probe.os_pid_liveness(entry.os_pid)? {
        SupervisorPidLiveness::Alive => Ok(SupervisorPidFinding::Fresh {
            target,
            os_pid: entry.os_pid,
        }),
        SupervisorPidLiveness::Dead => Ok(SupervisorPidFinding::Stale {
            target,
            recorded: Some(entry),
            reason: SupervisorPidStaleReason::RecordedPidDead,
        }),
    }
}

/// Classifies a target the registry cannot back at the target
/// generation: the resolver is the only remaining pid source, and its
/// failure to help leaves the drift / missing verdict.
fn classify_resolver_backed_target(
    target: SupervisorPidScanTarget,
    recorded: Option<SupervisorPidEntry>,
    probe: &mut impl SupervisorPidProbe,
) -> Result<SupervisorPidFinding, SupervisorError> {
    match probe.resolve_os_pid(target) {
        Some(candidate) => match probe.os_pid_liveness(candidate)? {
            SupervisorPidLiveness::Alive => Ok(SupervisorPidFinding::Fresh {
                target,
                os_pid: candidate,
            }),
            SupervisorPidLiveness::Dead => Ok(SupervisorPidFinding::Stale {
                target,
                recorded,
                reason: SupervisorPidStaleReason::CandidatePidDead { candidate },
            }),
        },
        None => match recorded {
            Some(entry) => Ok(SupervisorPidFinding::Stale {
                target,
                recorded: Some(entry),
                reason: SupervisorPidStaleReason::GenerationDrift {
                    recorded_generation: entry.process_generation,
                },
            }),
            None => Ok(SupervisorPidFinding::Missing { target }),
        },
    }
}
