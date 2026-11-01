//! Dedicated sandbox host binary (`ADR-0015` layer-1 carrier process).
//!
//! This is the **only** production process that ever loads an untrusted
//! Wasm module: `nlos-wasm-sandbox`'s library API orchestrates this binary
//! through [`nlos_wasm_sandbox::process`], spawning it with an empty
//! environment, explicit argv, and piped stdio. It reads zero environment
//! variables outside the explicit `--self-audit-env` attestation mode.

#![forbid(unsafe_code)]

fn main() {
    let argv: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let code = nlos_wasm_sandbox::host_bin::host_main(&argv);
    std::process::exit(code);
}
