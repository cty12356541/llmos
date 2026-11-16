use std::time::Duration;

use nlos_wasm_sandbox::process::{HostProcessRequest, run_isolated_host};
use nlos_wasm_sandbox::{HostOutcome, SandboxProfile};

#[test]
fn drains_both_child_pipes_while_waiting() {
    let path = std::env::temp_dir().join(format!("nlos-tool-pipes-{}.wat", std::process::id()));
    std::fs::write(
        &path,
        r#"(module
            (import "wasi_snapshot_preview1" "fd_write"
                (func $write (param i32 i32 i32 i32) (result i32)))
            (memory (export "memory") 2)
            (data (i32.const 57359) "\0a")
            (func (export "main") (result i32) (local $remaining i32)
                (local.set $remaining (i32.const 30))
                (i32.store (i32.const 0) (i32.const 16))
                (i32.store (i32.const 4) (i32.const 57344))
                (loop $output
                (drop (call $write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))
                (drop (call $write (i32.const 2) (i32.const 0) (i32.const 1) (i32.const 8)))
                (local.set $remaining (i32.sub (local.get $remaining) (i32.const 1)))
                (br_if $output (local.get $remaining)))
                (i32.const 0)))"#,
    )
    .unwrap();
    let request = HostProcessRequest::new(
        env!("CARGO_BIN_EXE_nlos-wasm-sandbox-host"),
        &path,
        SandboxProfile::deny_all(),
    );
    let result = run_isolated_host(&request, Some(Duration::from_secs(3)));
    std::fs::remove_file(path).unwrap();
    let result = result.unwrap();
    assert!(!result.timed_out_and_killed);
    assert_eq!(result.report.unwrap().outcome, HostOutcome::Completed);
    assert_eq!(result.stdout.len(), 30 * 57344);
    assert!(result.stderr.len() > 30 * 57344);
}
