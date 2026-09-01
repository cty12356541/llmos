//! `slice-k-demo` — single-process sequential run of the first
//! longitudinal slice: happy chain, cancel path, crash recovery
//! (drop + reopen), and the authority-sourced inspect view. One stable
//! `[slice-k]`-prefixed line per step; every receipt line is grep-friendly.

use std::sync::Arc;

use nlos_types::PackageId;

use nlos_application::DisableApplicationRequest;
use nlos_runtime::RuntimeAdapter as _;
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig};
use nlos_slice_k::{
    ChainQuery, HappyChain, SliceKRuntime, run_cancel_path, run_happy_chain, run_recovery_prefix,
    seeded_key, short_hex,
};

fn receipt_line(kind: &str, id: &[u8], detail: &str) {
    println!(
        "[slice-k] RECEIPT kind={kind} id={} {detail}",
        short_hex(id)
    );
}

#[tokio::main]
async fn main() {
    let root = std::env::temp_dir().join(format!("nlos-slice-k-demo-{}", std::process::id()));
    println!("[slice-k] runtime root {}", root.display());

    let runtime = Arc::new(SliceKRuntime::open(&root).expect("open slice-k runtime"));
    let adapter = TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig::default(),
    )
    .expect("tokio adapter");

    demo_happy_chain(&runtime, &adapter).await;
    demo_cancel_path(&runtime, &adapter).await;
    demo_recovery(runtime, adapter, &root).await;
    println!("[slice-k] DONE");

    let _ = std::fs::remove_dir_all(&root);
}

async fn demo_happy_chain(runtime: &Arc<SliceKRuntime>, adapter: &TokioRuntimeAdapter) {
    println!("[slice-k] STEP 01 sign-package begin");
    let chain = run_happy_chain(runtime, adapter, 0x2A)
        .await
        .expect("happy chain");
    println!(
        "[slice-k] STEP 01 sign-package done package={} signer={}",
        short_hex(chain.package.package_id.as_bytes()),
        short_hex(chain.publisher.principal_id.as_bytes())
    );
    receipt_line(
        "package-verification",
        chain.verification_receipt_id.as_bytes(),
        &format!(
            "package={} entries=1",
            short_hex(chain.package.package_id.as_bytes())
        ),
    );
    receipt_line(
        "installation",
        chain.installation_id.as_bytes(),
        &format!("application={}", short_hex(chain.application_id.as_bytes())),
    );
    receipt_line(
        "commit-permit",
        chain.permit_id.as_bytes(),
        &format!(
            "task={} attempt={}",
            short_hex(chain.task_id.as_bytes()),
            short_hex(chain.attempt_id.as_bytes())
        ),
    );
    println!(
        "[slice-k] STEP 05b materialize-process done process={} generation={} agent={} domain={}",
        short_hex(chain.process.process_id.as_bytes()),
        chain.process.process_generation.get(),
        short_hex(chain.process.agent_instance_id.as_bytes()),
        short_hex(chain.process.isolation_domain_id.as_bytes()),
    );
    println!(
        "[slice-k] STEP 06 fiber-operation operation={} plan={} fiber={:?}",
        short_hex(chain.outcome.operation.operation_id.as_bytes()),
        short_hex(chain.plan_id.as_bytes()),
        adapter.inspect(chain.fiber).expect("fiber state")
    );
    receipt_line(
        "task-commit",
        chain.receipt.task_receipt.receipt_id.as_bytes(),
        &format!(
            "task={} head={} publications={}",
            short_hex(chain.task_id.as_bytes()),
            chain.receipt.task_receipt.new_head_commit_seq,
            chain.receipt.artifact_publications.len()
        ),
    );
    print_inspect(runtime, &chain, "[slice-k] INSPECT");
    demo_lifecycle(runtime, &chain);
}

fn print_inspect(runtime: &SliceKRuntime, chain: &HappyChain, prefix: &str) {
    println!("[slice-k] STEP 09 inspect begin");
    let inspect = runtime
        .inspect_chain(ChainQuery {
            package_id: chain.package.package_id,
            installation_id: Some(chain.installation_id),
            process_id: Some(chain.process.process_id),
            task_id: chain.task_id,
            attempt_id: chain.attempt_id,
            permit_id: Some(chain.permit_id),
            artifact_id: chain.package.payload_artifact,
            operation: Some(chain.outcome.operation),
        })
        .expect("inspect chain");
    for line in inspect.report_lines() {
        println!("{prefix} {line}");
    }
}

/// Demonstrates the §23.1 lifecycle tail on an installed application: a
/// same-package reinstall advances the installation generation, then
/// `disable_application` lands the terminal status and further installs
/// fail closed.
fn demo_lifecycle(runtime: &Arc<SliceKRuntime>, chain: &HappyChain) {
    println!("[slice-k] STEP 09b lifecycle begin");
    let reinstall = runtime
        .install_verified_package_by_id(chain.verification_receipt_id, 0x2D)
        .expect("reinstall advances the generation");
    println!(
        "[slice-k] STEP 09b reinstall-advance application={} generation={}",
        short_hex(chain.application_id.as_bytes()),
        reinstall.installation_generation.get()
    );
    let disabled_at_ms = u64::try_from(
        runtime
            .wall_now_i64(seeded_key(0x2A, 90))
            .expect("wall for disable"),
    )
    .expect("wall fits u64");
    let disable = runtime
        .applications
        .disable_application(DisableApplicationRequest {
            package_id: chain.package.package_id,
            idempotency_key: seeded_key(0x2A, 91),
            disabled_at_ms,
        })
        .expect("disable the happy-chain application");
    let disable_receipt = disable.receipt();
    println!(
        "[slice-k] STEP 09b disable application={} generation={} at_ms={}",
        short_hex(disable_receipt.application_id.as_bytes()),
        disable_receipt.application_generation.get(),
        disable_receipt.disabled_at_ms
    );
    assert!(
        runtime
            .install_verified_package_by_id(chain.verification_receipt_id, 0x2E)
            .is_err(),
        "installing over a disabled application must fail closed"
    );
    println!("[slice-k] STEP 09b reinstall-after-disable refused (fail-closed)");
}

async fn demo_cancel_path(runtime: &Arc<SliceKRuntime>, adapter: &TokioRuntimeAdapter) {
    println!("[slice-k] STEP 10 cancel-path begin");
    let cancel = run_cancel_path(runtime, adapter, 0x2B)
        .await
        .expect("cancel path");
    let nlos_task::CancelDecision::Applied {
        cancel_epoch,
        closed_attempts,
    } = cancel.cancel
    else {
        panic!("cancel path: fresh task must cancel as Applied");
    };
    println!(
        "[slice-k] STEP 10 cancel applied epoch={cancel_epoch} closed_attempts={}",
        closed_attempts.len()
    );
    println!(
        "[slice-k] STEP 11 permit-after-cancel {:?}",
        cancel.fenced_permit
    );
    println!(
        "[slice-k] STEP 12 task_state={:?} converged_plans={}",
        runtime
            .tasks
            .inspect_task(cancel.task_id)
            .expect("cancelled task")
            .state,
        cancel.converged_plans
    );
}

async fn demo_recovery(
    runtime: Arc<SliceKRuntime>,
    adapter: TokioRuntimeAdapter,
    root: &std::path::Path,
) {
    println!("[slice-k] STEP 13 recovery-prefix begin");
    let prefix = run_recovery_prefix(&runtime, &adapter, 0x2C)
        .await
        .expect("recovery prefix");
    println!(
        "[slice-k] STEP 13 prefix durable task={} plan={} (not converged)",
        short_hex(prefix.task_id.as_bytes()),
        short_hex(prefix.plan_id.as_bytes())
    );
    let pre_task_head = runtime
        .tasks
        .inspect_task(prefix.task_id)
        .expect("pre-crash task")
        .head_commit_seq;
    drop(adapter);
    drop(runtime);
    println!(
        "[slice-k] STEP 14 crash-drop all authorities dropped (pre-crash task head {pre_task_head})"
    );

    let reopened = SliceKRuntime::open(root).expect("reopen slice-k runtime");
    println!(
        "[slice-k] STEP 14 reopen root {}",
        reopened.root().display()
    );
    let survived = reopened
        .process
        .inspect_active_process_binding(prefix.process.process_id)
        .expect("process binding survived the crash");
    println!(
        "[slice-k] STEP 14 process-binding survived process={} generation={}",
        short_hex(survived.process_id.as_bytes()),
        survived.process_generation.get(),
    );
    let now_ms = reopened
        .wall_now_i64(seeded_key(0x2C, 99))
        .expect("post-reopen wall reading");
    let receipts = reopened
        .converge_pending(16, now_ms)
        .expect("converge after reopen");
    let recovered = receipts
        .iter()
        .find(|receipt| receipt.task_receipt.task_id == prefix.task_id)
        .expect("recovered receipt for the prefix task");
    receipt_line(
        "task-commit-recovered",
        recovered.task_receipt.receipt_id.as_bytes(),
        &format!(
            "task={} head={} publications={}",
            short_hex(prefix.task_id.as_bytes()),
            recovered.task_receipt.new_head_commit_seq,
            recovered.artifact_publications.len()
        ),
    );
    let post = reopened
        .inspect_chain(ChainQuery {
            package_id: PackageId::from_bytes([0x2C; 16]),
            installation_id: Some(prefix.installation_id),
            process_id: Some(prefix.process.process_id),
            task_id: prefix.task_id,
            attempt_id: prefix.attempt_id,
            permit_id: Some(prefix.permit_id),
            artifact_id: prefix.artifact_id,
            operation: Some(prefix.operation),
        })
        .expect("inspect recovered chain");
    for line in post.report_lines() {
        println!("[slice-k] INSPECT-RECOVERED {line}");
    }
}
