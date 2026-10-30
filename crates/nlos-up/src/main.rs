//! CLI entry point scaffold for `nlos-up`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let _ = nlos_up::CRATE_NAME;
    ExitCode::SUCCESS
}
