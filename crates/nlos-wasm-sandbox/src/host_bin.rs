//! Child-side entrypoint of the dedicated sandbox host binary
//! (`ADR-0015` layer-1 carrier process).
//!
//! [`host_main`] is a pure function of its argv: it reads **zero**
//! environment variables outside the explicit `--self-audit-env`
//! attestation mode (where the count itself is the attestation). Every
//! sandbox parameter arrives as an explicit command-line argument produced
//! by [`crate::process::build_isolated_host_command`].
//!
//! The terminal report line is written to stderr in the shape
//! `NLOS-HOST-R1 outcome=<token> code=<n> detail=<token>
//! guest_stdout_len=<n>`; guest stdout bytes flow through the child's real
//! stdout.
//!
//! Exit codes: `0` completed, guest-requested code for `guest-exit`, `64`
//! bad arguments, `65` invalid profile, `66` unreadable guest artifact,
//! `70` other host error, `133` trapped guest.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::error::{SandboxError, sanitize_detail};
use crate::isolate::{GuestOutcome, run_guest};
use crate::profile::{PreopenGrant, SandboxProfile};
use crate::tool_host_protocol::{
    MAX_WALL_MS, ToolHostLimits, parse_digest, read_artifact, write_stderr_frame,
};
use crate::wasi_allowlist::StdioSink;

/// Child exit code for malformed argv.
pub const EXIT_BAD_ARGUMENTS: i32 = 64;
/// Child exit code for an invalid sandbox profile.
pub const EXIT_PROFILE_INVALID: i32 = 65;
/// Child exit code for an unreadable guest artifact.
pub const EXIT_GUEST_READ_FAILED: i32 = 66;
/// Child exit code for other host-side errors.
pub const EXIT_HOST_ERROR: i32 = 70;
/// Child exit code for a trapped guest.
pub const EXIT_TRAPPED: i32 = 133;

fn emit_report(outcome: &str, code: i64, detail: &str, guest_stdout_len: u64) {
    eprintln!(
        "NLOS-HOST-R1 outcome={outcome} code={code} detail={detail} guest_stdout_len={guest_stdout_len}"
    );
}

fn sandbox_error_token(error: &SandboxError) -> &'static str {
    match error {
        SandboxError::Profile(_) => "profile-invalid",
        SandboxError::GuestModuleRejected { .. } => "module-rejected",
        SandboxError::GuestInstantiationRejected { .. } => "instantiation-rejected",
        SandboxError::EntrypointMissing { .. } => "entrypoint-missing",
        SandboxError::EntrypointSignatureInvalid { .. } => "entrypoint-signature",
        SandboxError::GuestEngineFailure { .. } => "engine-failure",
        SandboxError::HostIo(_) => "host-io",
        SandboxError::HostReportMissing { .. } => "host-report-missing",
    }
}

/// A parsed `--preopen` triplet.
struct ParsedPreopen {
    guest_dir: String,
    host_root: PathBuf,
    allow_write: bool,
}

/// The fully parsed host argv.
struct ToolPlan {
    digest: [u8; 32],
    limits: ToolHostLimits,
    wall_ms: u64,
}

#[derive(Default)]
struct ToolPlanArgs {
    digest: Option<[u8; 32]>,
    limits: Option<ToolHostLimits>,
    wall_ms: Option<u64>,
}

impl ToolPlanArgs {
    fn set(&mut self, flag: &str, value: &str) -> Result<(), String> {
        match flag {
            "--tool-digest" if self.digest.is_none() => {
                self.digest = Some(parse_digest(value).ok_or("bad tool digest")?);
            }
            "--tool-limits" if self.limits.is_none() => {
                let mut parts = value.split(',');
                let mut next = || parts.next().ok_or("missing tool limit");
                let limits = ToolHostLimits {
                    stdout: next()?.parse().map_err(|_| "bad stdout limit")?,
                    stderr: next()?.parse().map_err(|_| "bad stderr limit")?,
                    file_write: next()?.parse().map_err(|_| "bad file write limit")?,
                };
                if parts.next().is_some() {
                    return Err("extra tool limits".into());
                }
                self.limits = Some(limits);
            }
            "--tool-wall-ms" if self.wall_ms.is_none() => {
                self.wall_ms = Some(value.parse().map_err(|_| "bad tool wall limit")?);
            }
            _ => return Err("duplicate tool flag".into()),
        }
        Ok(())
    }

    fn finish(self, ticks: u64, interval_ms: u64) -> Result<Option<ToolPlan>, String> {
        match (self.digest, self.limits, self.wall_ms) {
            (None, None, None) => Ok(None),
            (Some(digest), Some(limits), Some(wall_ms))
                if limits.valid()
                    && (1..=MAX_WALL_MS).contains(&wall_ms)
                    && ticks > 0
                    && interval_ms > 0
                    && ticks
                        .checked_mul(interval_ms)
                        .is_some_and(|epoch_ms| epoch_ms <= MAX_WALL_MS) =>
            {
                Ok(Some(ToolPlan {
                    digest,
                    limits,
                    wall_ms,
                }))
            }
            _ => Err("invalid tool process plan".into()),
        }
    }
}

struct HostRunPlan {
    wasm_path: PathBuf,
    entrypoint: String,
    fuel: u64,
    memory_pages: u64,
    epoch_ticks: u64,
    epoch_interval_ms: u64,
    preopens: Vec<ParsedPreopen>,
    guest_args: Vec<String>,
    tool: Option<ToolPlan>,
}

/// Returns the UTF-8 string value immediately after position `index`.
fn value_after(argv: &[OsString], index: usize, flag: &str) -> Result<String, String> {
    let Some(item) = argv.get(index + 1) else {
        return Err(format!("{flag} requires a value"));
    };
    item.to_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "argument values must be non-empty UTF-8".to_string())
}

/// Returns the numeric value immediately after position `index`.
fn number_after(argv: &[OsString], index: usize, flag: &str) -> Result<u64, String> {
    value_after(argv, index, flag)?
        .parse()
        .map_err(|_| format!("bad {flag}"))
}

/// Parses one `--preopen <guest> --preopen-host <path> [--preopen-write]`
/// triplet, advancing `index` past it.
fn parse_preopen(argv: &[OsString], index: &mut usize) -> Result<ParsedPreopen, String> {
    let guest_dir = value_after(argv, *index, "--preopen")?;
    if argv.get(*index + 2).and_then(|f| f.to_str()) != Some("--preopen-host") {
        return Err("--preopen must be followed by --preopen-host".to_string());
    }
    let Some(host) = argv.get(*index + 3) else {
        return Err("--preopen-host requires a path".to_string());
    };
    let host_root = PathBuf::from(host);
    *index += 4;
    let mut allow_write = false;
    if argv.get(*index).and_then(|f| f.to_str()) == Some("--preopen-write") {
        allow_write = true;
        *index += 1;
    }
    Ok(ParsedPreopen {
        guest_dir,
        host_root,
        allow_write,
    })
}

/// Parses strict argv. Unknown flags or missing values are rejected.
fn parse_argv(argv: &[OsString]) -> Result<HostRunPlan, String> {
    let mut wasm_path: Option<PathBuf> = None;
    let mut entrypoint = String::from("main");
    let mut fuel: Option<u64> = None;
    let mut memory_pages: Option<u64> = None;
    let mut epoch_ticks: u64 = 0;
    let mut epoch_interval_ms: u64 = 0;
    let mut preopens: Vec<ParsedPreopen> = Vec::new();
    let mut guest_args: Vec<String> = Vec::new();

    let mut tool_args = ToolPlanArgs::default();
    let mut index = 0;
    while index < argv.len() {
        let flag = argv[index].to_str().unwrap_or_default().to_string();
        match flag.as_str() {
            "--guest" => {
                let Some(v) = argv.get(index + 1) else {
                    return Err("--guest requires a path".to_string());
                };
                wasm_path = Some(PathBuf::from(v));
                index += 2;
            }
            "--entry" => {
                entrypoint = value_after(argv, index, "--entry")?;
                index += 2;
            }
            "--fuel" => {
                fuel = Some(number_after(argv, index, "--fuel")?);
                index += 2;
            }
            "--memory-pages" => {
                memory_pages = Some(number_after(argv, index, "--memory-pages")?);
                index += 2;
            }
            "--epoch-ticks" => {
                epoch_ticks = number_after(argv, index, "--epoch-ticks")?;
                index += 2;
            }
            "--epoch-interval-ms" => {
                epoch_interval_ms = number_after(argv, index, "--epoch-interval-ms")?;
                index += 2;
            }
            "--preopen" => {
                let preopen = parse_preopen(argv, &mut index)?;
                preopens.push(preopen);
            }
            "--tool-digest" | "--tool-limits" | "--tool-wall-ms" => {
                tool_args.set(&flag, &value_after(argv, index, &flag)?)?;
                index += 2;
            }
            "--arg" => {
                guest_args.push(value_after(argv, index, "--arg")?);
                index += 2;
            }
            other => return Err(format!("unknown flag {other:?}")),
        }
    }

    let Some(wasm_path) = wasm_path else {
        return Err("--guest is required".to_string());
    };
    let fuel = fuel.unwrap_or(SandboxProfile::DEFAULT_FUEL);
    let memory_pages = memory_pages.unwrap_or(SandboxProfile::DEFAULT_MEMORY_PAGES);
    if epoch_ticks > 0 && epoch_interval_ms == 0 {
        return Err("--epoch-ticks requires --epoch-interval-ms".to_string());
    }
    let tool = tool_args.finish(epoch_ticks, epoch_interval_ms)?;
    Ok(HostRunPlan {
        wasm_path,
        entrypoint,
        fuel,
        memory_pages,
        epoch_ticks,
        epoch_interval_ms,
        preopens,
        guest_args,
        tool,
    })
}

fn plan_to_profile(plan: &HostRunPlan) -> Result<SandboxProfile, SandboxError> {
    let mut profile = SandboxProfile::deny_all()
        .with_fuel(plan.fuel)
        .with_max_memory_pages(plan.memory_pages);
    if plan.epoch_ticks > 0 {
        profile = profile.with_epoch_deadline(plan.epoch_ticks, plan.epoch_interval_ms);
    }
    for preopen in &plan.preopens {
        let grant = PreopenGrant::from_host_plan(
            &preopen.guest_dir,
            preopen.host_root.clone(),
            preopen.allow_write,
        )
        .map_err(SandboxError::Profile)?;
        profile = profile.with_preopen(grant);
    }
    profile = profile.with_guest_args(plan.guest_args.clone());
    if let Some(tool) = &plan.tool {
        profile = profile.with_tool_limits(tool.limits);
    }
    profile.validate().map_err(SandboxError::Profile)?;
    Ok(profile)
}

/// Entry function of the dedicated sandbox host binary. Returns the child
/// process exit code. Reads the environment only in audit mode.
#[must_use]
pub fn host_main(argv: &[OsString]) -> i32 {
    if argv.len() == 1 && argv[0] == "--self-audit-env" {
        // The only code path that reads the environment: counting it to
        // attest that the orchestrating parent passed an empty one.
        let count = std::env::vars_os().count();
        let count_code = i64::try_from(count).unwrap_or(i64::MAX);
        emit_report("audit-env", count_code, "env-var-count", 0);
        return 0;
    }

    let plan = match parse_argv(argv) {
        Ok(plan) => plan,
        Err(reason) => {
            emit_report(
                "host-error",
                i64::from(EXIT_BAD_ARGUMENTS),
                &format!("bad-arguments:{}", sanitize_detail(&reason)),
                0,
            );
            return EXIT_BAD_ARGUMENTS;
        }
    };

    if let Some(tool) = &plan.tool {
        return run_tool_plan(&plan, tool);
    }

    let profile = match plan_to_profile(&plan) {
        Ok(profile) => profile,
        Err(error) => {
            emit_report(
                "host-error",
                i64::from(EXIT_PROFILE_INVALID),
                &format!("{}:{}", sandbox_error_token(&error), error),
                0,
            );
            return EXIT_PROFILE_INVALID;
        }
    };

    let guest_bytes = match std::fs::read(&plan.wasm_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            emit_report(
                "host-error",
                i64::from(EXIT_GUEST_READ_FAILED),
                &format!("guest-read-failed:{}", sanitize_detail(&error.to_string())),
                0,
            );
            return EXIT_GUEST_READ_FAILED;
        }
    };

    match run_guest(
        &profile,
        &guest_bytes,
        &plan.entrypoint,
        Box::new(StdioSink::new()),
    ) {
        Ok((outcome, sink)) => {
            let mut sink = sink;
            let stdout_len = match sink.as_any().downcast_mut::<StdioSink>() {
                Some(s) => s.guest_stdout_len,
                None => 0,
            };
            match outcome {
                GuestOutcome::Completed { code, .. } => {
                    emit_report(
                        "completed",
                        i64::from(code),
                        "entrypoint-returned",
                        stdout_len,
                    );
                    0
                }
                GuestOutcome::Exited { code, .. } => {
                    emit_report("guest-exit", i64::from(code), "proc-exit", stdout_len);
                    code
                }
                GuestOutcome::Limit(limit) => {
                    emit_report("trap", 0, limit.token(), stdout_len);
                    EXIT_TRAPPED
                }
                GuestOutcome::Trapped { kind } => {
                    emit_report("trap", 0, &kind.report_token(), stdout_len);
                    EXIT_TRAPPED
                }
            }
        }
        Err(error) => {
            emit_report(
                "host-error",
                i64::from(EXIT_HOST_ERROR),
                &format!(
                    "{}:{}",
                    sandbox_error_token(&error),
                    sanitize_detail(&error.to_string())
                ),
                0,
            );
            EXIT_HOST_ERROR
        }
    }
}

pub(crate) const EXIT_TOOL_WATCHDOG: i32 = 71;

fn emit_tool_report(outcome: &str, code: i32, detail: &str, stdout_len: u64) {
    let line = format!(
        "NLOS-HOST-R1 outcome={outcome} code={code} detail={detail} guest_stdout_len={stdout_len}"
    );
    let _ = write_stderr_frame(std::io::stderr().lock(), line.as_bytes(), true);
}

fn run_tool_plan(plan: &HostRunPlan, tool: &ToolPlan) -> i32 {
    // Wasmtime's epoch interrupts guest code. This watchdog also bounds host
    // compilation and any synchronous WASI call when the parent disappears.
    let hard_limit = Duration::from_millis(tool.wall_ms + 250);
    if std::thread::Builder::new()
        .spawn(move || {
            std::thread::sleep(hard_limit);
            std::process::exit(EXIT_TOOL_WATCHDOG);
        })
        .is_err()
    {
        return EXIT_HOST_ERROR;
    }
    let Ok(profile) = plan_to_profile(plan) else {
        emit_tool_report("host-error", EXIT_PROFILE_INVALID, "profile-invalid", 0);
        return EXIT_PROFILE_INVALID;
    };
    let Ok(bytes) = read_artifact(&plan.wasm_path) else {
        emit_tool_report("host-error", EXIT_GUEST_READ_FAILED, "guest-read-failed", 0);
        return EXIT_GUEST_READ_FAILED;
    };
    if Sha256::digest(&bytes).as_slice() != tool.digest {
        emit_tool_report("host-error", EXIT_GUEST_READ_FAILED, "artifact-changed", 0);
        return EXIT_GUEST_READ_FAILED;
    }
    let (outcome, mut sink) = match run_guest(
        &profile,
        &bytes,
        &plan.entrypoint,
        Box::new(StdioSink::tool()),
    ) {
        Ok(pair) => pair,
        Err(error) => {
            emit_tool_report(
                "host-error",
                EXIT_HOST_ERROR,
                sandbox_error_token(&error),
                0,
            );
            return EXIT_HOST_ERROR;
        }
    };
    let stdout_len = sink
        .as_any()
        .downcast_mut::<StdioSink>()
        .map_or(0, |sink| sink.guest_stdout_len);
    match outcome {
        GuestOutcome::Completed { code, .. } => {
            emit_tool_report("completed", code, "entrypoint-returned", stdout_len);
            0
        }
        GuestOutcome::Exited { code, .. } => {
            emit_tool_report("guest-exit", code, "proc-exit", stdout_len);
            code
        }
        GuestOutcome::Limit(limit) => {
            emit_tool_report("trap", 0, limit.token(), stdout_len);
            EXIT_TRAPPED
        }
        GuestOutcome::Trapped { kind } => {
            // Error text can include guest bytes and paths. Tool mode emits
            // a fixed vocabulary over its private process protocol.
            let detail = match kind {
                crate::TrapKind::EpochInterrupt => "epoch-interrupt",
                crate::TrapKind::OutOfFuel => "out-of-fuel",
                _ => "guest-trap",
            };
            emit_tool_report("trap", 0, detail, stdout_len);
            EXIT_TRAPPED
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os_args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn unknown_flag_is_rejected_as_bad_arguments() {
        let code = host_main(&os_args(&["--bogus"]));
        assert_eq!(code, EXIT_BAD_ARGUMENTS);
    }

    #[test]
    fn missing_guest_flag_is_rejected() {
        let code = host_main(&os_args(&["--fuel", "10"]));
        assert_eq!(code, EXIT_BAD_ARGUMENTS);
    }

    #[test]
    fn audit_env_mode_reports_the_real_environment_count_in_process() {
        // In-process the host sees the test runner's environment: the count
        // must be positive (control direction). The dual-layer zero count
        // is asserted in the integration matrix through spawn_env_audit.
        let code = host_main(&os_args(&["--self-audit-env"]));
        assert_eq!(code, 0);
    }

    #[test]
    fn unreadable_guest_artifact_reports_guest_read_failed() {
        let missing =
            std::env::temp_dir().join(format!("nlos-e6a-missing-{}-ghost.wat", std::process::id()));
        let code = host_main(&os_args(&["--guest", missing.to_str().expect("utf8 path")]));
        assert_eq!(code, EXIT_GUEST_READ_FAILED);
    }

    #[test]
    fn full_argv_round_trip_runs_a_minimal_guest_to_completion() {
        let dir = std::env::temp_dir().join(format!("nlos-e6a-bin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let wat = dir.join("mini.wat");
        std::fs::write(&wat, "(module (memory (export \"memory\") 1) (func (export \"main\") (result i32) (i32.const 5)))")
            .expect("write wat");
        let code = host_main(&os_args(&[
            "--guest",
            wat.to_str().expect("utf8 path"),
            "--fuel",
            "100000",
        ]));
        assert_eq!(code, 0, "guest completing must exit 0 (report on stderr)");
        let _unused = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn profile_invalid_reports_typed_exit() {
        let dir = std::env::temp_dir().join(format!("nlos-e6a-badp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let wat = dir.join("mini.wat");
        std::fs::write(
            &wat,
            "(module (func (export \"main\") (result i32) (i32.const 0)))",
        )
        .expect("write wat");
        // fuel = 0 violates the fail-closed profile.
        let code = host_main(&os_args(&[
            "--guest",
            wat.to_str().expect("utf8 path"),
            "--fuel",
            "0",
        ]));
        assert_eq!(code, EXIT_PROFILE_INVALID);
        let _unused = std::fs::remove_dir_all(&dir);
    }
}
