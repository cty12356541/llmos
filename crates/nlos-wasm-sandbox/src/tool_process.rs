use std::io::{self, Read};
use std::process::{Child, Command};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::error::{HostReport, SandboxError};
use crate::process::{HostProcessRequest, build_isolated_host_command, stop_and_reap};
use crate::tool_host_protocol::{self, FrameError, ToolHostLimits};
use crate::tool_operation_store::StoreError;

const DURABLE_CONTROL_POLL: Duration = Duration::from_millis(20);

#[derive(Default)]
pub(crate) struct ChildControl(Mutex<ChildState>);

#[derive(Default)]
struct ChildState {
    cancelled: bool,
    child: Option<Child>,
}

impl ChildControl {
    pub fn cancel(&self) -> io::Result<()> {
        let mut state = self.0.lock().map_err(|_| io::Error::other("child state"))?;
        state.cancelled = true;
        Self::stop(&mut state)
    }

    fn stop(state: &mut ChildState) -> io::Result<()> {
        if let Some(child) = state.child.as_mut() {
            stop_and_reap(child)?;
        }
        Ok(())
    }

    fn wait(
        &self,
        deadline: Instant,
        pipe_failed: &AtomicBool,
        observe_control: &mut impl FnMut() -> Result<(), HostStop>,
    ) -> io::Result<Result<Option<i32>, HostStop>> {
        let mut next_observation = Instant::now();
        loop {
            {
                let mut state = self.0.lock().map_err(|_| io::Error::other("child state"))?;
                if state.cancelled {
                    Self::stop(&mut state)?;
                    return Ok(Err(HostStop::Cancelled));
                }
                if Instant::now() >= deadline {
                    Self::stop(&mut state)?;
                    return Ok(Err(HostStop::Deadline));
                }
                if pipe_failed.load(Ordering::Acquire) {
                    Self::stop(&mut state)?;
                    return Ok(Ok(None));
                }
                let child = state.child.as_mut().expect("host was spawned");
                if let Some(status) = child.try_wait()? {
                    return Ok(Ok(status.code()));
                }
            }
            // A replaceable store may take an I/O lock. Never hold the child
            // lock across an operation lookup.
            if Instant::now() >= next_observation {
                if let Err(stop) = observe_control() {
                    self.cancel()?;
                    return Ok(Err(stop));
                }
                next_observation = Instant::now() + DURABLE_CONTROL_POLL;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HostStop {
    Deadline,
    Cancelled,
    StdoutLimit,
    StderrLimit,
    Protocol,
    Control(StoreError),
}

pub(crate) struct ToolHostOutcome {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub report: Option<HostReport>,
}

pub(crate) fn command(
    request: &HostProcessRequest,
    artifact_digest: [u8; 32],
    limits: ToolHostLimits,
    wall_ms: u64,
) -> Result<Command, SandboxError> {
    let mut command = build_isolated_host_command(request)?;
    command
        .arg("--tool-digest")
        .arg(tool_host_protocol::digest_hex(artifact_digest))
        .arg("--tool-limits")
        .arg(format!(
            "{},{},{}",
            limits.stdout, limits.stderr, limits.file_write
        ))
        .arg("--tool-wall-ms")
        .arg(wall_ms.to_string());
    Ok(command)
}

pub(crate) fn run(
    control: &ChildControl,
    command: &mut Command,
    deadline: Instant,
    limits: ToolHostLimits,
    mut observe_control: impl FnMut() -> Result<(), HostStop>,
) -> io::Result<Result<ToolHostOutcome, HostStop>> {
    let (stdout, stderr) = {
        let mut state = control
            .0
            .lock()
            .map_err(|_| io::Error::other("child state"))?;
        if state.cancelled {
            return Ok(Err(HostStop::Cancelled));
        }
        if Instant::now() >= deadline {
            return Ok(Err(HostStop::Deadline));
        }
        let mut child = command.spawn()?;
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            let _ = stop_and_reap(&mut child);
            return Err(io::Error::other("missing host pipes"));
        };
        state.child = Some(child);
        (stdout, stderr)
    };

    let pipe_failed = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let out = std::thread::Builder::new().spawn_scoped(scope, || {
            let result = read_stdout(stdout, limits.stdout);
            if result.is_err() {
                pipe_failed.store(true, Ordering::Release);
            }
            result
        });
        let err = std::thread::Builder::new().spawn_scoped(scope, || {
            let result = tool_host_protocol::read_stderr_frames(stderr, limits.stderr);
            if result.is_err() {
                pipe_failed.store(true, Ordering::Release);
            }
            result
        });
        let wait = if out.is_ok() && err.is_ok() {
            control.wait(deadline, &pipe_failed, &mut observe_control)
        } else {
            Err(io::Error::other("starting host pipe readers failed"))
        };
        if wait.is_err() {
            let _ = control.cancel();
        }
        let code = wait?;
        let stdout = out?
            .join()
            .map_err(|_| io::Error::other("stdout reader failed"))?;
        let report = err?
            .join()
            .map_err(|_| io::Error::other("stderr reader failed"))?;
        Ok((|| {
            Ok(ToolHostOutcome {
                code: code?,
                stdout: stdout?,
                report: report.map_err(|failure| match failure {
                    FrameError::Limit => HostStop::StderrLimit,
                    FrameError::Invalid => HostStop::Protocol,
                })?,
            })
        })())
    })
}

fn read_stdout(mut pipe: impl Read, limit: usize) -> Result<Vec<u8>, HostStop> {
    let mut out = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let len = match pipe.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result.map_err(|_| HostStop::Protocol)?,
        };
        if len == 0 {
            return Ok(out);
        }
        if len > limit - out.len() {
            return Err(HostStop::StdoutLimit);
        }
        out.extend_from_slice(&buffer[..len]);
    }
}
