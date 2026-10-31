//! CLI entry point for `nlos-migrate` (Stage E E3 / `ADR-0027`).
#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    println!(
        "{} v{}",
        nlos_migrate::CRATE_NAME,
        env!("CARGO_PKG_VERSION")
    );
    ExitCode::SUCCESS
}
