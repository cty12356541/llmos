//! Standalone isolated host subprocess binary wrapping
//! [`nlos_wasm_sandbox::host_bin::host_main`] for the detached
//! `stage-e-road-e001-upgrade-gate` workspace (`[TCB-BYPASS-001]`, `U-9`).

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let code = nlos_wasm_sandbox::host_bin::host_main(&args);
    let clamped = u8::try_from(code.clamp(0, 255)).unwrap_or(1);
    ExitCode::from(clamped)
}
