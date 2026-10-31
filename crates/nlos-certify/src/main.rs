//! `nlos-certify` CLI binary entrypoint (Stage E E5 four-gate certification pipeline).

#![forbid(unsafe_code)]

use std::process::ExitCode;

use nlos_certify::CERTIFY_PIPELINE_GATE_COUNT;

fn main() -> ExitCode {
    println!(
        "usage: nlos-certify <package-dir> ({CERTIFY_PIPELINE_GATE_COUNT}-gate certification scaffold)"
    );
    ExitCode::SUCCESS
}
