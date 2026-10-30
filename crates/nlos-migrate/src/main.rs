//! CLI entry point for `nlos-migrate`.

#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    println!("{}", nlos_migrate::CRATE_NAME);
    ExitCode::SUCCESS
}
