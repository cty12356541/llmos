//! B-SLICE-K-001: the first longitudinal slice end to end.
//!
//! Three tests, one per scenario lane:
//!
//! 1. `full_vertical_slice_produces_every_receipt_and_is_inspectable` —
//!    signed Package → verify → install Application → Task/Attempt →
//!    `CommitPermit` → tokio fiber (durable driver Operation + staged
//!    revision + commit plan) → converge → `TaskCommitReceipt` → inspect.
//! 2. `cancel_closes_attempt_fences_permit_and_runtime_scope` — the cancel
//!    path with no commit appearing.
//! 3. `drop_reopen_replays_durable_prefix_to_consistent_terminal_state` —
//!    the crash-recovery analogue: every authority dropped mid-chain,
//!    reopened over the same root, converged to the identical durable
//!    terminal state, idempotent under a second drain.

use std::sync::Arc;

use nlos_application::ApplicationStatus;
use nlos_artifact::{
    ContentDigest, PackageVerificationDecision, VerifyPackageRequest, package_manifest_message,
};
use nlos_runtime::{FiberExit, FiberSpec, FiberState, RuntimeAdapter as _, RuntimeError};
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig};
use nlos_slice_k::{
    ChainQuery, SliceKRuntime, run_cancel_path, run_happy_chain, run_recovery_prefix, seeded_key,
};
use nlos_task::{AttemptState, CancelDecision, PermitDecision, PermitState, TaskState};
use nlos_types::{ExecutionFiberId, Generation, ResourceGroupId, SchedulerDomainId};

fn slice_runtime(name: &str) -> (TempDir, Arc<SliceKRuntime>) {
    let dir = TempDir::new(name);
    let runtime = Arc::new(SliceKRuntime::open(dir.root()).expect("open slice-k runtime"));
    (dir, runtime)
}

fn slice_adapter() -> TokioRuntimeAdapter {
    TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig::default(),
    )
    .expect("tokio adapter")
}

struct TempDir {
    root: std::path::PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-slice-k-{name}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create temp root");
        Self { root }
    }

    fn root(&self) -> &std::path::Path {
        &self.root
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove slice-k temp root: {error}"),
        }
    }
}

#[tokio::test]
async fn full_vertical_slice_produces_every_receipt_and_is_inspectable() {
    let (_dir, runtime) = slice_runtime("happy");
    let adapter = slice_adapter();

    let chain = run_happy_chain(&runtime, &adapter, 0xA0)
        .await
        .expect("happy chain");

    // The digest-binding ladder, one authority per step.
    let verification = runtime
        .artifacts
        .inspect_package_verification_receipt(chain.verification_receipt_id)
        .expect("verification receipt readback");
    assert_eq!(verification.signer, chain.publisher.principal_id);
    assert_eq!(
        verification.manifest_digest,
        ContentDigest::from_bytes(package_manifest_message(&chain.package.manifest))
    );

    let installation = runtime
        .applications
        .inspect_installation(chain.installation_id)
        .expect("installation readback");
    assert_eq!(
        installation.package_verification_receipt_id,
        chain.verification_receipt_id
    );
    assert_eq!(
        installation.package_manifest_digest,
        verification.manifest_digest
    );

    // The fiber's write landed as revision 2 (the package payload is 1).
    let head = runtime
        .artifacts
        .resolve_head(chain.package.payload_artifact)
        .expect("head readback")
        .expect("head after commit");
    assert_eq!(head.revision, 2);

    // Terminal task facts: permit consumed, head advanced, attempt committed.
    let task = runtime.tasks.inspect_task(chain.task_id).expect("task");
    assert_eq!(task.head_commit_seq, 1);
    assert_eq!(task.state, TaskState::Active);
    let attempt = runtime
        .tasks
        .inspect_attempt(chain.task_id, chain.attempt_id)
        .expect("attempt");
    assert_eq!(attempt.state, AttemptState::Committed);
    let permit = runtime
        .tasks
        .inspect_permit(chain.task_id, chain.permit_id)
        .expect("permit");
    assert_eq!(permit.state, PermitState::Closed);

    // The receipt binds the whole ladder.
    assert_eq!(chain.receipt.task_receipt.task_id, chain.task_id);
    assert_eq!(chain.receipt.task_receipt.permit_id, Some(chain.permit_id));
    assert_eq!(chain.receipt.artifact_publications.len(), 1);

    // The fiber ran under an authority-registered process binding, not a
    // fabricated id: the durable record readback agrees with the spec.
    assert_eq!(chain.process.process_generation, Generation::INITIAL);
    let binding = runtime
        .process
        .inspect_active_process_binding(chain.process.process_id)
        .expect("process binding readback");
    assert_eq!(binding, chain.process);
    assert_eq!(binding.task_id, chain.task_id);
    assert_eq!(binding.task_attempt_id, chain.attempt_id);

    // The application is durably installed.
    let application = runtime
        .applications
        .inspect_application(chain.package.package_id)
        .expect("application readback")
        .expect("installed application");
    assert_eq!(application.status, ApplicationStatus::Installed);
    assert_eq!(application.application_id, chain.application_id);

    // In-process inspect sees the same facts from the authorities alone.
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
        .expect("inspect");
    assert_eq!(inspect.task.head_commit_seq, 1);
    assert_eq!(inspect.artifact_head.as_ref().expect("head").revision, 2);
    assert_eq!(
        inspect.application.as_ref().expect("app").status,
        ApplicationStatus::Installed
    );
    assert_eq!(
        inspect.process.as_ref().expect("process binding"),
        &chain.process
    );
    let lines = inspect.report_lines();
    assert!(lines.iter().any(|line| line.contains("head_commit_seq=1")));
    assert!(lines.iter().any(|line| line.contains("revision=2")));
    assert!(lines.iter().any(|line| line.contains(&format!(
        "process={} generation={}",
        nlos_slice_k::short_hex(chain.process.process_id.as_bytes()),
        chain.process.process_generation.get()
    ))));
}

#[tokio::test]
async fn cancel_closes_attempt_fences_permit_and_runtime_scope() {
    let (_dir, runtime) = slice_runtime("cancel");
    let adapter = slice_adapter();

    let facts = run_cancel_path(&runtime, &adapter, 0xB0)
        .await
        .expect("cancel path");

    let CancelDecision::Applied {
        cancel_epoch,
        closed_attempts,
    } = facts.cancel
    else {
        panic!("fresh task must cancel as Applied");
    };
    assert_eq!(cancel_epoch, 1);
    assert_eq!(closed_attempts.len(), 1);
    assert_eq!(closed_attempts[0].attempt_id, facts.attempt_id);

    // The task authority durably fences any later permit request.
    assert!(matches!(
        facts.fenced_permit,
        PermitDecision::CancelledBeforeEffect { .. }
    ));

    // The runtime scope of the cancelled attempt refuses new fibers.
    let fiber_spec = FiberSpec {
        fiber_id: ExecutionFiberId::from_bytes([0xB0u8.wrapping_add(200); 16]),
        fiber_generation: Generation::INITIAL,
        agent_instance_id: facts.process.agent_instance_id,
        agent_generation: facts.process.agent_instance_generation,
        process_id: facts.process.process_id,
        process_generation: facts.process.process_generation,
        task_attempt_id: Some(facts.attempt_id),
        cancellation_scope_id: facts.scope_id,
        cancellation_generation: Generation::INITIAL,
        resource_group_id: ResourceGroupId::from_bytes([0xB3; 16]),
        scheduler_domain_id: SchedulerDomainId::from_bytes([0xB4; 16]),
        deadline: None,
    };
    assert!(matches!(
        adapter.spawn_fiber(fiber_spec, Box::pin(async { FiberExit::Completed })),
        Err(RuntimeError::Cancelled)
    ));

    // Durable terminal state: cancelled task, no commit anywhere.
    let task = runtime.tasks.inspect_task(facts.task_id).expect("task");
    assert_eq!(task.state, TaskState::Cancelled);
    assert_eq!(task.cancel_epoch, 1);
    assert_eq!(task.head_commit_seq, 0);
    let attempt = runtime
        .tasks
        .inspect_attempt(facts.task_id, facts.attempt_id)
        .expect("attempt");
    assert_eq!(attempt.state, AttemptState::Cancelled);
    assert_eq!(facts.converged_plans, 0, "no plan ever existed");
    assert_eq!(
        adapter.inspect(facts.fiber).expect("fiber state"),
        FiberState::Completed,
        "the operation-only prefix completed before the cancel"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn drop_reopen_replays_durable_prefix_to_consistent_terminal_state() {
    let (dir, runtime) = slice_runtime("recovery");
    let adapter = slice_adapter();

    // Pre-crash: everything through the fiber's durable prefix, no converge.
    let prefix = run_recovery_prefix(&runtime, &adapter, 0xC0)
        .await
        .expect("recovery prefix");
    let pre_verification = runtime
        .artifacts
        .inspect_package_verification_receipt(prefix.verification_receipt_id)
        .expect("pre-crash verification receipt");
    let pre_installation = runtime
        .applications
        .inspect_installation(prefix.installation_id)
        .expect("pre-crash installation");
    let pre_task = runtime
        .tasks
        .inspect_task(prefix.task_id)
        .expect("pre-crash task");
    assert_eq!(pre_task.head_commit_seq, 0, "not converged yet");
    let pre_head = runtime
        .artifacts
        .resolve_head(prefix.artifact_id)
        .expect("pre-crash head")
        .expect("package head");
    assert_eq!(pre_head.revision, 1, "only the package payload head");
    let pre_operation = runtime
        .operations
        .inspect(prefix.operation)
        .expect("pre-crash operation");
    let pre_operation_state = pre_operation.state;
    let pre_binding = runtime
        .process
        .inspect_active_process_binding(prefix.process.process_id)
        .expect("pre-crash process binding");
    assert_eq!(pre_binding, prefix.process);

    // kill -9 analogue: the runtime adapter and every authority handle are
    // dropped without any close; the durable bytes under the root survive.
    drop(adapter);
    drop(runtime);
    let root = dir.root().to_path_buf();

    // Reopen every authority over the same root.
    let reopened = Arc::new(SliceKRuntime::open(&root).expect("reopen slice-k runtime"));

    // Nothing advanced, nothing vanished: the durable prefix is whole.
    let reopened_task = reopened.tasks.inspect_task(prefix.task_id).expect("task");
    assert_eq!(reopened_task.head_commit_seq, 0);
    assert_eq!(
        reopened
            .artifacts
            .resolve_head(prefix.artifact_id)
            .expect("head")
            .expect("head")
            .revision,
        1
    );
    assert_eq!(
        reopened
            .applications
            .inspect_installation(prefix.installation_id)
            .expect("installation"),
        pre_installation
    );
    let reopened_operation = reopened
        .operations
        .inspect(prefix.operation)
        .expect("reopened operation");
    assert_eq!(reopened_operation.state, pre_operation_state);

    // The process binding is a durable fact of the crash too: the reopened
    // authority readback returns the identical binding with an unchanged
    // generation, and the same registration replays idempotently.
    let reopened_binding = reopened
        .process
        .inspect_active_process_binding(prefix.process.process_id)
        .expect("reopened process binding");
    assert_eq!(reopened_binding, pre_binding);
    assert_eq!(
        reopened_binding.process_generation,
        prefix.process.process_generation
    );
    let replay = reopened
        .materialize_process(0xC0, prefix.task_id, prefix.attempt_id, Generation::INITIAL)
        .expect("replay process materialization after reopen");
    assert_eq!(replay, prefix.process);

    // Fiber replay/收敛: the coordinator replays the durable prefix
    // (staged revision + commit plan) to the terminal state.
    let now_ms = reopened
        .wall_now_i64(seeded_key(0xC0, 95))
        .expect("post-reopen wall reading");
    let receipts = reopened
        .converge_pending(16, now_ms)
        .expect("converge after reopen");
    assert_eq!(receipts.len(), 1, "exactly the prefix plan finalizes");
    let receipt = &receipts[0];
    assert_eq!(receipt.task_receipt.task_id, prefix.task_id);
    assert_eq!(receipt.task_receipt.permit_id, Some(prefix.permit_id));
    assert_eq!(receipt.task_receipt.new_head_commit_seq, 1);
    assert_eq!(receipt.artifact_publications.len(), 1);

    // Durable terminal state agrees with the receipt everywhere.
    let post_task = reopened.tasks.inspect_task(prefix.task_id).expect("task");
    assert_eq!(post_task.head_commit_seq, 1);
    let post_attempt = reopened
        .tasks
        .inspect_attempt(prefix.task_id, prefix.attempt_id)
        .expect("attempt");
    assert_eq!(post_attempt.state, AttemptState::Committed);
    let post_head = reopened
        .artifacts
        .resolve_head(prefix.artifact_id)
        .expect("head")
        .expect("head");
    assert_eq!(post_head.revision, 2);
    assert_eq!(
        post_head.digest.as_bytes(),
        &receipt.artifact_publications[0].digest
    );

    // A second drain is a no-op: no double commit.
    let later = reopened
        .wall_now_i64(seeded_key(0xC0, 96))
        .expect("wall reading");
    assert!(
        reopened
            .converge_pending(16, later)
            .expect("second drain")
            .is_empty()
    );

    // The package verification receipt is the durable authority after the
    // crash too: the same verify request replays byte-identically.
    let reopened_publisher = reopened.bootstrap_publisher(0xC0).expect("publisher");
    let decision = reopened
        .artifacts
        .verify_package(
            &reopened.identity,
            VerifyPackageRequest {
                signed: &prefix.signed,
                idempotency_key: seeded_key(0xC0, 14),
                verified_at_ms: pre_verification.verified_at_ms,
            },
        )
        .expect("verify replay after reopen");
    assert!(matches!(decision, PackageVerificationDecision::Replayed(_)));
    assert_eq!(decision.receipt(), &pre_verification);
    assert_eq!(
        reopened_publisher.principal_id, pre_verification.signer,
        "identity authority reopened with the same principal"
    );
}
