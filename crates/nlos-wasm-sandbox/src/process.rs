//! Layer-1 host-process orchestration (`ADR-0015` decision 1; `[TCB-BYPASS-001]`,
//! `[TCB-EGRESS-001]`, `[DRV-LIFE-001]`).
//!
//! The library itself never loads an untrusted module into the caller's
//! process. Production callers hand a [`HostProcessRequest`] to
//! [`run_isolated_host`], which spawns the **dedicated sandbox host binary**
//! ([`crate::host_bin`]) with controlled parameters:
//!
//! * **Zero environment inheritance**: the child is spawned with
//!   `.env_clear()` — an empty environment block. The host binary reads no
//!   environment variables at all except in its explicit
//!   `--self-audit-env` attestation mode, where it counts them (and
//!   observes zero).
//! * **Explicit argv only**: every sandbox parameter (guest path,
//!   entrypoint, fuel, memory pages, epoch deadline, preopens, guest argv)
//!   crosses the boundary as an explicit command-line argument. No
//!   configuration is ever taken from the environment.
//! * **Stdio**: stdin is `null` (guests observe EOF), stdout and stderr are
//!   piped to the parent. Guest stdout bytes flow through the child's real
//!   stdout; the child's own structured `NLOS-HOST-R1` report line is
//!   written to its stderr.
//! * **Working directory**: set explicitly to the guest artifact's parent
//!   directory (never inherited implicitly).
//!
//! # Platform handle-inheritance boundaries (documented honestly)
//!
//! * **Windows**: `std::process` on Windows creates the child with
//!   `bInheritHandles` set only for the stdio handles the parent explicitly
//!   wired (here: the two pipes; stdin is a null handle). No other handles
//!   of the parent are inheritable unless the parent itself marked them
//!   inheritable; this crate marks nothing inheritable.
//! * **Unix**: every descriptor Rust opens is `FD_CLOEXEC` by default, so
//!   the child inherits exactly the stdio descriptors wired above and
//!   nothing else.
//! * **Residual boundary**: an *ambiently* inheritable descriptor opened by
//!   unrelated parent code (marked inheritable outside this crate) is out
//!   of this crate's control on either platform; callers must not mark
//!   descriptors inheritable. No socket is ever created in the child.
//!
//! [`run_isolated_host`] adds a wall-clock deadline: on expiry the child is
//! hard-killed (`TerminateProcess` on Windows, `SIGKILL` on Unix) — the
//! terminal last resort of `ADR-0015` decision 1 — and the outcome is
//! reported with [`HostProcessOutcome::timed_out_and_killed`].

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{HostOutcome, HostReport, SandboxError};
use crate::profile::SandboxProfile;

/// One request to execute an untrusted guest in an isolated host process.
#[derive(Debug, Clone)]
pub struct HostProcessRequest {
    host_exe: PathBuf,
    wasm_path: PathBuf,
    entrypoint: String,
    profile: SandboxProfile,
    work_dir: Option<PathBuf>,
}

impl HostProcessRequest {
    /// Creates a request for the given host binary, guest artifact, and
    /// profile. The entrypoint defaults to `main`.
    #[must_use]
    pub fn new(
        host_exe: impl Into<PathBuf>,
        wasm_path: impl Into<PathBuf>,
        profile: SandboxProfile,
    ) -> Self {
        Self {
            host_exe: host_exe.into(),
            wasm_path: wasm_path.into(),
            entrypoint: "main".to_string(),
            profile,
            work_dir: None,
        }
    }

    /// Overrides the guest entrypoint export name.
    #[must_use]
    pub fn with_entrypoint(mut self, entrypoint: impl Into<String>) -> Self {
        self.entrypoint = entrypoint.into();
        self
    }

    /// Overrides the child working directory (default: the guest
    /// artifact's parent directory).
    #[must_use]
    pub fn with_work_dir(mut self, work_dir: impl Into<PathBuf>) -> Self {
        self.work_dir = Some(work_dir.into());
        self
    }

    /// The dedicated host binary to spawn.
    #[must_use]
    pub fn host_exe(&self) -> &Path {
        &self.host_exe
    }

    /// The untrusted guest artifact path handed to the child.
    #[must_use]
    pub fn wasm_path(&self) -> &Path {
        &self.wasm_path
    }
}

/// Builds the controlled `std::process::Command` for one guest run:
/// explicit argv, empty environment, null stdin, piped stdout/stderr, and
/// an explicit working directory.
///
/// # Errors
///
/// Returns [`SandboxError::Profile`] if the profile fails fail-closed
/// validation before anything is spawned.
pub fn build_isolated_host_command(request: &HostProcessRequest) -> Result<Command, SandboxError> {
    request.profile.validate().map_err(SandboxError::Profile)?;
    let mut command = Command::new(&request.host_exe);
    command
        .arg("--guest")
        .arg(&request.wasm_path)
        .arg("--entry")
        .arg(&request.entrypoint)
        .arg("--fuel")
        .arg(request.profile.fuel().to_string())
        .arg("--memory-pages")
        .arg(request.profile.max_memory_pages().to_string())
        .arg("--epoch-ticks")
        .arg(request.profile.epoch_deadline_ticks().to_string())
        .arg("--epoch-interval-ms")
        .arg(request.profile.epoch_tick_interval_ms().to_string());
    for grant in request.profile.preopens() {
        command
            .arg("--preopen")
            .arg(grant.guest_dir())
            .arg("--preopen-host")
            .arg(grant.host_root());
        if grant.allow_write() {
            command.arg("--preopen-write");
        }
    }
    for guest_arg in request.profile.guest_args() {
        command.arg("--arg").arg(guest_arg);
    }
    let work_dir = request
        .work_dir
        .clone()
        .or_else(|| request.wasm_path.parent().map(Path::to_path_buf))
        .unwrap_or_else(std::env::temp_dir);
    command
        .env_clear()
        .current_dir(work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command)
}

/// Runs the `--self-audit-env` attestation mode of the host binary under
/// the same isolation discipline (empty environment, piped stdio) and
/// returns the report, whose `code` is the number of environment variables
/// the child observed.
///
/// # Errors
///
/// Returns [`SandboxError::HostIo`] on spawn/wait failure or
/// [`SandboxError::HostReportMissing`] when no parsable report was emitted.
pub fn spawn_env_audit(host_exe: impl AsRef<Path>) -> Result<HostProcessOutcome, SandboxError> {
    let mut command = Command::new(host_exe.as_ref());
    command
        .arg("--self-audit-env")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().map_err(SandboxError::HostIo)?;
    let output = child.wait_with_output().map_err(SandboxError::HostIo)?;
    let report = HostReport::parse(&String::from_utf8_lossy(&output.stderr));
    Ok(HostProcessOutcome {
        exit_code: exit_code_of(&output),
        timed_out_and_killed: false,
        stdout: output.stdout,
        stderr: output.stderr,
        report,
    })
}

fn exit_code_of(output: &std::process::Output) -> Option<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        output.status.code().or_else(|| output.status.signal())
    }
    #[cfg(windows)]
    {
        output.status.code()
    }
}

/// The observed result of one isolated host-process run.
#[derive(Debug)]
pub struct HostProcessOutcome {
    /// The child exit code, when it exited by itself (`None` when killed).
    pub exit_code: Option<i32>,
    /// Whether the parent had to hard-kill the child after the wall-clock
    /// deadline expired.
    pub timed_out_and_killed: bool,
    /// Raw child stdout bytes (guest stdout flows through here).
    pub stdout: Vec<u8>,
    /// Raw child stderr bytes (contains the `NLOS-HOST-R1` report line).
    pub stderr: Vec<u8>,
    /// The structured report parsed from stderr, when present.
    pub report: Option<HostReport>,
}

/// Spawns the isolated host process and collects its output, optionally
/// enforcing a wall-clock deadline with a hard kill as the terminal last
/// resort.
///
/// # Errors
///
/// Returns [`SandboxError::Profile`] for an invalid profile,
/// [`SandboxError::HostIo`] for spawn/wait/kill failures.
pub fn run_isolated_host(
    request: &HostProcessRequest,
    deadline: Option<Duration>,
) -> Result<HostProcessOutcome, SandboxError> {
    let mut child = build_isolated_host_command(request)?
        .spawn()
        .map_err(SandboxError::HostIo)?;
    let start = Instant::now();
    let mut timed_out_and_killed = false;
    if let Some(deadline) = deadline {
        loop {
            if child.try_wait().map_err(SandboxError::HostIo)?.is_some() {
                break;
            }
            if start.elapsed() >= deadline {
                child.kill().map_err(SandboxError::HostIo)?;
                timed_out_and_killed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let output = child.wait_with_output().map_err(SandboxError::HostIo)?;
    let report = HostReport::parse(&String::from_utf8_lossy(&output.stderr));
    if report.is_none() && !timed_out_and_killed {
        let tail = crate::error::sanitize_detail(&String::from_utf8_lossy(&output.stderr));
        return Err(SandboxError::HostReportMissing { stderr_tail: tail });
    }
    Ok(HostProcessOutcome {
        exit_code: exit_code_of(&output),
        timed_out_and_killed,
        stdout: output.stdout,
        stderr: output.stderr,
        report,
    })
}

impl HostReport {
    /// Parses the last `NLOS-HOST-R1 outcome=<t> code=<n> detail=<d>
    /// guest_stdout_len=<n>` line from host stderr.
    #[must_use]
    pub fn parse(stderr_text: &str) -> Option<HostReport> {
        let line = stderr_text
            .lines()
            .rev()
            .find(|l| l.starts_with("NLOS-HOST-R1 "))?;
        let mut outcome = None;
        let mut code = 0;
        let mut detail = String::new();
        let mut guest_stdout_len = 0;
        for field in line.split_whitespace().skip(1) {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };
            match key {
                "outcome" => outcome = HostOutcome::parse_token(value),
                "code" => code = value.parse().unwrap_or(0),
                "detail" => detail = value.to_string(),
                "guest_stdout_len" => guest_stdout_len = value.parse().unwrap_or(0),
                _ => {}
            }
        }
        Some(HostReport {
            outcome: outcome?,
            code,
            detail,
            guest_stdout_len,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_command_carries_explicit_argv_and_empty_environment() {
        let root = std::env::temp_dir().join("nlos-e6a-cmd-probe-root");
        std::fs::create_dir_all(&root).expect("root dir");
        let grant = crate::profile::PreopenGrant::new("/workspace", &root, false).expect("grant");
        let request = HostProcessRequest::new(
            "host-probe.exe",
            "guest.wat",
            SandboxProfile::deny_all()
                .with_fuel(1234)
                .with_max_memory_pages(8)
                .with_guest_args(["alpha"])
                .with_preopen(grant),
        );
        let command = build_isolated_host_command(&request).expect("command");
        let argv: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let joined = argv.join(" ");
        assert!(joined.contains("--fuel 1234"), "argv was {joined}");
        assert!(joined.contains("--memory-pages 8"), "argv was {joined}");
        assert!(joined.contains("--preopen /workspace"), "argv was {joined}");
        assert!(joined.contains("--arg alpha"), "argv was {joined}");
        assert_eq!(command.get_envs().count(), 0, "environment must be empty");
        assert_eq!(command.get_program().to_string_lossy(), "host-probe.exe");
    }

    #[test]
    fn report_line_parses_round_trip() {
        let stderr =
            "noise line\nNLOS-HOST-R1 outcome=trap code=0 detail=out-of-fuel guest_stdout_len=7\n";
        let report = HostReport::parse(stderr).expect("parses");
        assert_eq!(report.outcome, HostOutcome::Trap);
        assert_eq!(report.code, 0);
        assert_eq!(report.detail, "out-of-fuel");
        assert_eq!(report.guest_stdout_len, 7);
        assert!(HostReport::parse("no report here").is_none());
    }
}
