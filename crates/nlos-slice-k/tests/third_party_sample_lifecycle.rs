//! W33-B (B1-5): the third-party sample application's full lifecycle,
//! headless, through public surfaces only — the mechanical twin of
//! `examples/sample-app/lifecycle.sh` (which drives the real
//! `nlos-package` CLI + `sample-app-driver` binaries across processes).
//!
//! No kernel-internal API appears anywhere in this file: every step is the
//! public surface of the landed authorities or the `nlos-slice-k`
//! assembly. The chain, one lane per lifecycle phase:
//!
//! 1. **develop/sign/verify**: a two-entry package whose `tasks` segment
//!    carries the W28-B template face (an `agent-role` node and an
//!    `executable` node depending on it) is built, signed, and verified
//!    through `verify_package_with_tasks`; `compile_task_templates` maps
//!    the signed segment onto the plan-authority proposal shape
//!    (`[PLAN-OVERRIDE-001]`: the segment is a declaration *source*, not a
//!    second dialect).
//! 2. **install**: the verification receipt id feeds
//!    `install_verified_package` (generation 1, status `installed`).
//! 3. **run**: a Task/Attempt pair whose durable rows carry the
//!    application association, a delegated Process, a `CommitPermit`, a
//!    fiber's durable driver Operation plus a staged revision and commit
//!    plan over an application-owned output artifact, converged to a
//!    `TaskCommitReceipt` (head revision 2); the background-task and
//!    process-binding registrations land against the application, and the
//!    W27-D uninstall activity gate refuses typed while the Task is
//!    outstanding.
//! 4. **update**: the same package at `1.1.0` (same major) verifies to a
//!    distinct receipt; a cross-major `2.0.0` target is refused by the
//!    `SameMajor` window with zero durable state; the honest target then
//!    walks the W29-E migration runner (begin → 2 steps → health verdict
//!    with a probe consulted exactly once → atomic switch to generation 2).
//! 5. **uninstall**: the W30-D teardown chain drives the registered
//!    process binding through the platform-kill chain and the background
//!    Task to `Cancelled`, the gate opens, the uninstall commits, and a
//!    re-run replays byte-identical receipts while STILL re-signaling
//!    through the supervisor registry (at-least-once: the dead service
//!    stand-in reports `AlreadyTerminated` — success).
//!
//! Unix runs the kill chain against a real OS child (`sleep 600`) fed to
//! the supervisor registry — the documented stand-in for the sample's
//! declared `background-service` payload; non-Unix hosts run the same
//! chain against the noop contract adapter.

use std::cell::Cell;
use std::future::pending;
use std::sync::Arc;

use ed25519_dalek::Signer;
use nlos_application::{
    ActivateMigrationDecision, ActivatePackageMigrationRequest, ApplicationAuthorityError,
    ApplicationStatus, CompatibilityWindow, MigrateApplicationRequest, MigrateDecision,
    MigrationHealthContext, MigrationHealthDecision, MigrationHealthProbe, MigrationState,
    RecordMigrationStepDecision, RecordMigrationStepRequest, compile_task_templates,
    pack_package_version,
};
use nlos_artifact::{
    ContentDigest, CreateArtifactSpec, PackageEntryRole, PackageManifest, PackageManifestEntry,
    PackageTaskKind, PackageTaskTemplate, ProvenanceSourceTriple, PutRevisionRequest,
    SignedPackageWithTasks, VerifyPackageWithTasksRequest, package_manifest_with_tasks_message,
    staging_id_for,
};
use nlos_process::{
    PlatformKillDecision, ProcessLifecycleState, RegisterSupervisorPidRequest,
    SupervisorPidRegistry,
};
use nlos_runtime::{FiberSpec, FiberState, RuntimeAdapter as _, RuntimeError};
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig};
use nlos_slice_k::{
    SliceKError, SliceKRuntime, WriteFiberJob, run_application_teardown, spawn_write_fiber,
};
use nlos_task::{
    ArtifactPublicationExpectation, Authorities, CancelDecision, PermitDecision, PermitRequest,
    artifact_publication_plan_root,
};
use nlos_types::{
    ApplicationId, ArtifactId, CallbackId, Generation, IdempotencyKey, OperationId, PackageId,
    ReceiptId, ResourceGroupId, SchedulerDomainId,
};

/// The sample package identity (mirrors `examples/sample-app/package/`).
const PACKAGE: [u8; 16] = [
    0x9a, 0x1c, 0x7d, 0x3e, 0x5f, 0x0b, 0x2a, 0x4c, 0x6d, 0x8e, 0x0f, 0x1a, 0x2b, 0x3c, 0x4d, 0x5e,
];
/// The segment's two declaration keys (main node, service node).
const NODE_MAIN: [u8; 16] = [
    0x01, 0xfa, 0x3c, 0x57, 0xb9, 0xd1, 0xe2, 0xf4, 0xa6, 0xb8, 0xc0, 0xd2, 0xe4, 0xf6, 0xa8, 0xb0,
];
const NODE_SERVICE: [u8; 16] = [
    0x02, 0xce, 0x7a, 0x19, 0xd5, 0xf3, 0xb1, 0xa9, 0xc7, 0xe5, 0xd3, 0xf1, 0xb9, 0xa7, 0xc5, 0xe3,
];

/// Seed of the slice-k reuse helpers (task/attempt/process/registration
/// bands live at `seed + 20..33` / `seed + 110..114`).
const CHAIN_SEED: u8 = 0x10;

/// One tag byte → one idempotency-key band (`[tag; 16]`, the migration
/// test's fixture convention). Tags 0x41..0x4F name idempotency keys,
/// 0x51..0x5F name clock keys — disjoint from every helper seed band and
/// from each other's authorities by construction of this table:
///
/// ```text
/// 0x41 v1 entry artifact create      0x51 v1 entry publish clock
/// 0x42 run output artifact create    0x52 run output publish clock
/// 0x43 v1.1 entry artifact create    0x53 v1.1 entry publish clock
/// 0x44 v2 entry artifact create      0x54 v2 entry publish clock
/// 0x45 v1 verify idempotency         0x55 v1 verify clock
/// 0x46 v1.1 verify idempotency       0x56 v1.1 verify clock
/// 0x47 v2 verify idempotency         0x57 v2 verify clock
/// 0x48 migration drill key           0x58 run permit clock
/// 0x49 run permit idempotency        0x59 fiber stage clock
/// 0x4A fiber stage key               0x5A fiber plan clock
/// 0x4B fiber plan key                0x5B converge clock
/// 0x4C cross-major drill key         0x5C supervisor registration clock
/// 0x4D step-1 clock                  0x5D template-compile clock
/// 0x4E plan-proposal key             0x5E migration begin clock
/// 0x4F step-2 clock                  0x5F health-check clock
///                                     0x63 cross-major begin clock
///                                     0x6A migration activation clock
/// ```
fn k(tag: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([tag; 16])
}

fn id(tag: u8) -> [u8; 16] {
    [tag; 16]
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
            "nlos-slice-k-sample-{name}-{}-{sequence}",
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
            Err(error) => panic!("remove sample temp root: {error}"),
        }
    }
}

fn adapter() -> TokioRuntimeAdapter {
    TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig::default(),
    )
    .expect("tokio adapter")
}

/// One signed, verified task-templated package revision: a single
/// executable entry whose payload carries the version tag, plus the
/// two-template segment (service depends on main).
struct VerifiedRevision {
    receipt_id: ReceiptId,
    manifest_digest: ContentDigest,
    package_version: u64,
    signed: SignedPackageWithTasks,
}

fn templates(version_tag: &str) -> Vec<PackageTaskTemplate> {
    vec![
        PackageTaskTemplate {
            node_key: NODE_MAIN,
            kind: PackageTaskKind::AgentRole,
            binding_digest: ContentDigest::of_bytes(
                format!("main-binding-{version_tag}").as_bytes(),
            )
            .into_bytes(),
            dependency_keys: Vec::new(),
            input_selectors_digest: ContentDigest::of_bytes(b"input-selectors").into_bytes(),
            output_contract_digest: ContentDigest::of_bytes(b"output-contract").into_bytes(),
            policy_digest: ContentDigest::of_bytes(b"policy").into_bytes(),
            resource_ceiling_digest: ContentDigest::of_bytes(b"resource-ceiling").into_bytes(),
        },
        PackageTaskTemplate {
            node_key: NODE_SERVICE,
            kind: PackageTaskKind::Executable,
            binding_digest: ContentDigest::of_bytes(
                format!("service-binding-{version_tag}").as_bytes(),
            )
            .into_bytes(),
            dependency_keys: vec![NODE_MAIN],
            input_selectors_digest: ContentDigest::of_bytes(b"service-inputs").into_bytes(),
            output_contract_digest: ContentDigest::of_bytes(b"service-outputs").into_bytes(),
            policy_digest: ContentDigest::of_bytes(b"service-policy").into_bytes(),
            resource_ceiling_digest: ContentDigest::of_bytes(b"service-ceiling").into_bytes(),
        },
    ]
}

/// Builds, signs, and verifies one revision through the same public
/// pipeline the `nlos-package` CLI's verify half rides: materialize the
/// entry payload into the artifact store, sign the with-tasks manifest
/// digest, run the authoritative `verify_package_with_tasks`.
fn verified_revision(
    runtime: &SliceKRuntime,
    signer: &nlos_slice_k::Publisher,
    version: u64,
    version_tag: &str,
    tags: (u8, u8, u8, u8),
) -> VerifiedRevision {
    let (create_tag, verify_tag, publish_clock, verify_clock) = tags;
    let artifact_id = ArtifactId::from_bytes(id(create_tag));
    let payload = format!("sample-app payload {version_tag}\n").into_bytes();
    let at_ms = runtime
        .wall_now_ms(k(publish_clock))
        .expect("entry publish clock");
    runtime
        .artifacts
        .create_artifact(CreateArtifactSpec {
            artifact_id,
            idempotency_key: k(create_tag),
            content_type: "application/octet-stream".to_string(),
            application_id: None,
            owner: None,
            created_at_ms: at_ms,
        })
        .expect("create entry artifact");
    runtime
        .artifacts
        .put_revision(PutRevisionRequest {
            artifact_id,
            expected_head_revision: 0,
            bytes: &payload,
            created_at_ms: at_ms,
            provenance: ProvenanceSourceTriple {
                source_a: id(create_tag),
                source_b: PACKAGE,
                source_digest: ContentDigest::of_bytes(&payload),
            },
        })
        .expect("put entry revision");
    let manifest = PackageManifest {
        package_id: PackageId::from_bytes(PACKAGE),
        version,
        entries: vec![PackageManifestEntry {
            name: "sample-driver".to_string(),
            artifact_id,
            digest: ContentDigest::of_bytes(&payload),
            role: PackageEntryRole::Executable,
        }],
    };
    let tasks = templates(version_tag);
    let message = package_manifest_with_tasks_message(&manifest, &tasks);
    let envelope = SignedPackageWithTasks {
        manifest: manifest.clone(),
        tasks,
        signer: signer.principal_id,
        signature: signer.signing.sign(&message).to_bytes(),
    };
    let decision = runtime
        .artifacts
        .verify_package_with_tasks(
            &runtime.identity,
            VerifyPackageWithTasksRequest {
                signed: &envelope,
                idempotency_key: k(verify_tag),
                verified_at_ms: runtime.wall_now_ms(k(verify_clock)).expect("verify clock"),
            },
        )
        .expect("verify templated package");
    let receipt = decision.receipt().clone();
    assert_eq!(receipt.manifest_digest, ContentDigest::from_bytes(message));
    assert_eq!(receipt.package_version, version);
    assert_eq!(receipt.signer, signer.principal_id);
    VerifiedRevision {
        receipt_id: receipt.receipt_id,
        manifest_digest: receipt.manifest_digest,
        package_version: version,
        signed: envelope,
    }
}

/// The health probe of the migration drill: counts consultations (the
/// durable verdict must never re-consult) and captures the context slice
/// the assertions need.
struct CountingProbe {
    calls: Cell<u32>,
    seen_steps: Cell<(u64, u64)>,
}

impl MigrationHealthProbe for CountingProbe {
    fn target_revision_healthy(&self, context: &MigrationHealthContext<'_>) -> bool {
        self.calls.set(self.calls.get() + 1);
        self.seen_steps
            .set((context.completed_step_count, context.declared_step_count));
        true
    }
}

/// Shared body; `service_child` carries the real OS child on Unix.
#[allow(clippy::too_many_lines)]
async fn third_party_sample_lifecycle_body(
    dir: &TempDir,
    mut service_child: Option<&mut std::process::Child>,
) {
    let runtime = Arc::new(SliceKRuntime::open(dir.root()).expect("open slice-k runtime"));
    let tokio_adapter = adapter();

    // ---- 1. develop → sign → verify (W28-B template face) ----
    let publisher = runtime.bootstrap_publisher(0xA0).expect("publisher");
    let v1 = verified_revision(
        &runtime,
        &publisher,
        pack_package_version(1, 0, 0),
        "v1.0.0",
        (0x41, 0x45, 0x51, 0x55),
    );
    let v11 = verified_revision(
        &runtime,
        &publisher,
        pack_package_version(1, 1, 0),
        "v1.1.0",
        (0x43, 0x46, 0x53, 0x56),
    );
    let v2 = verified_revision(
        &runtime,
        &publisher,
        pack_package_version(2, 0, 0),
        "v2.0.0",
        (0x44, 0x47, 0x54, 0x57),
    );
    assert_ne!(v1.manifest_digest, v11.manifest_digest);
    assert_ne!(v1.receipt_id, v11.receipt_id);

    // The signed segment compiles onto the plan-authority proposal shape
    // (`[PLAN-OVERRIDE-001]`: field-for-field, declaration order kept).
    let proposal = compile_task_templates(
        &v1.signed,
        k(0x4E),
        runtime.wall_now_ms(k(0x5D)).expect("compile clock"),
    )
    .expect("compile task templates");
    assert!(proposal.plan_id.is_none());
    assert_eq!(proposal.nodes.len(), 2);
    assert_eq!(proposal.nodes[0].node_key, NODE_MAIN);
    assert_eq!(proposal.nodes[1].node_key, NODE_SERVICE);
    assert_eq!(proposal.nodes[1].dependency_keys, vec![NODE_MAIN]);
    assert_eq!(
        proposal.nodes[0].binding_digest,
        v1.signed.tasks[0].binding_digest
    );
    assert_eq!(
        proposal.nodes[1].resource_ceiling_digest,
        v1.signed.tasks[1].resource_ceiling_digest
    );
    assert_eq!(format!("{:?}", proposal.nodes[0].kind), "AgentRole");
    assert_eq!(format!("{:?}", proposal.nodes[1].kind), "Executable");

    // ---- 2. install (receipt digest-binding) ----
    let installation = runtime
        .install_verified_package_by_id(v1.receipt_id, 0x13)
        .expect("install");
    let application_id: ApplicationId = installation.application_id;
    assert_eq!(installation.installation_generation.get(), 1);
    assert_eq!(installation.package_manifest_digest, v1.manifest_digest);
    let application = runtime
        .applications
        .inspect_application(PackageId::from_bytes(PACKAGE))
        .expect("inspect application")
        .expect("installed application");
    assert_eq!(application.status, ApplicationStatus::Installed);
    assert_eq!(application.application_id, application_id);

    // ---- 3. run (task/attempt/process/operation via public surfaces) ----
    let (task_id, attempt_id, scope_id) = runtime
        .register_task_and_attempt_for(CHAIN_SEED, Some(application_id), None)
        .expect("register task and attempt");
    let task_row = runtime.tasks.inspect_task(task_id).expect("task row");
    assert_eq!(task_row.application_id, Some(application_id));
    assert_eq!(task_row.plan_revision, None);

    let process = runtime
        .materialize_process(CHAIN_SEED, task_id, attempt_id, Generation::INITIAL)
        .expect("materialize process");

    let output_artifact = ArtifactId::from_bytes(id(0x42));
    let output_v1 = b"sample-app run ledger v1\n";
    let ledger_at = runtime.wall_now_ms(k(0x52)).expect("output clock");
    runtime
        .artifacts
        .create_artifact(CreateArtifactSpec {
            artifact_id: output_artifact,
            idempotency_key: k(0x42),
            content_type: "text/plain".to_string(),
            application_id: Some(application_id),
            owner: None,
            created_at_ms: ledger_at,
        })
        .expect("create output artifact");
    runtime
        .artifacts
        .put_revision(PutRevisionRequest {
            artifact_id: output_artifact,
            expected_head_revision: 0,
            bytes: output_v1,
            created_at_ms: ledger_at,
            provenance: ProvenanceSourceTriple {
                source_a: *application_id.as_bytes(),
                source_b: PACKAGE,
                source_digest: ContentDigest::of_bytes(output_v1),
            },
        })
        .expect("put output revision 1");

    let stage_key = k(0x4A);
    let output_v2: &[u8] = b"sample-app run output v1\n";
    let expectation = ArtifactPublicationExpectation {
        staging_id: staging_id_for(output_artifact, stage_key).into_bytes(),
        artifact_id: output_artifact,
        target_revision: 2,
        digest: ContentDigest::of_bytes(output_v2).into_bytes(),
        size_bytes: u64::try_from(output_v2.len()).unwrap_or(u64::MAX),
    };
    let write_set_root = artifact_publication_plan_root(&[expectation]).expect("plan root");
    let PermitDecision::Issued(permit) = runtime
        .tasks
        .request_commit_permit_with_authorities_struct(
            Authorities::default(),
            PermitRequest {
                task_id,
                attempt_id,
                attempt_generation: Generation::INITIAL,
                write_set_root,
                planned_effects: Vec::new(),
                idempotency_key: k(0x49),
                valid_until_ms: i64::MAX,
                requested_at_ms: runtime.wall_now_i64(k(0x58)).expect("permit clock"),
            },
        )
        .expect("permit")
    else {
        panic!("permit must be issued on a fresh task");
    };

    let job = WriteFiberJob {
        operation_id: OperationId::from_bytes(id(0x61)),
        callback_id: CallbackId::from_bytes(id(0x62)),
        completion_receipt_id: ReceiptId::from_bytes(id(0x63)),
        expected_head_revision: 1,
        artifact_id: output_artifact,
        stage_key,
        stage_bytes: output_v2.to_vec().into(),
        stage_created_at_ms: runtime.wall_now_ms(k(0x59)).expect("stage clock"),
        permit: Some(permit.permit_id),
        write_set_root,
        plan_key: k(0x4B),
        planned_at_ms: runtime.wall_now_i64(k(0x5A)).expect("plan clock"),
        task_id,
        attempt_id,
        attempt_generation: Generation::INITIAL,
    };
    let spec = FiberSpec {
        fiber_id: nlos_types::ExecutionFiberId::from_bytes(id(0x64)),
        fiber_generation: Generation::INITIAL,
        agent_instance_id: process.agent_instance_id,
        agent_generation: process.agent_instance_generation,
        process_id: process.process_id,
        process_generation: process.process_generation,
        task_attempt_id: Some(attempt_id),
        cancellation_scope_id: scope_id,
        cancellation_generation: Generation::INITIAL,
        resource_group_id: ResourceGroupId::from_bytes(id(0x65)),
        scheduler_domain_id: SchedulerDomainId::from_bytes(id(0x66)),
        deadline: None,
    };
    let (fiber, receiver) =
        spawn_write_fiber(Arc::clone(&runtime), &tokio_adapter, spec, job).expect("spawn fiber");
    let outcome = receiver.await.expect("fiber outcome").expect("fiber job");
    let plan_id = outcome
        .plan_id
        .expect("permit-bound fiber plans the commit");
    assert_eq!(plan_id.as_bytes().len(), 16);
    assert_eq!(
        tokio_adapter.inspect(fiber),
        Ok(FiberState::Completed),
        "the write fiber completed"
    );

    let receipts = runtime
        .converge_pending(16, runtime.wall_now_i64(k(0x5B)).expect("converge clock"))
        .expect("converge");
    let commit = receipts
        .iter()
        .find(|receipt| receipt.task_receipt.task_id == task_id)
        .expect("commit receipt for the run task");
    assert_eq!(commit.task_receipt.permit_id, Some(permit.permit_id));
    let head = runtime
        .artifacts
        .resolve_head(output_artifact, u64::MAX)
        .expect("head")
        .expect("head after commit");
    assert_eq!(head.revision, 2);
    assert_eq!(
        head.digest.as_bytes(),
        &commit.artifact_publications[0].digest
    );
    let attempt = runtime
        .tasks
        .inspect_attempt(task_id, attempt_id)
        .expect("attempt");
    assert_eq!(attempt.state, nlos_task::AttemptState::Committed);

    // The application's durable registrations (gate + teardown consumers).
    runtime
        .register_background_task(
            PackageId::from_bytes(PACKAGE),
            task_id,
            publisher.principal_id,
            CHAIN_SEED,
        )
        .expect("register background task");
    runtime
        .register_process_binding(
            PackageId::from_bytes(PACKAGE),
            process.process_id,
            publisher.principal_id,
            CHAIN_SEED,
        )
        .expect("register process binding");
    assert_eq!(
        runtime
            .tasks
            .inspect_outstanding_task_count(&[task_id])
            .expect("outstanding"),
        1,
        "the registered background task keeps the gate closed"
    );
    // 独立 seed 带（0x14）：拒绝探针的时钟键不得与 teardown 复用的
    // 0x12 带重叠——teardown 的 gated 卸载会按同键 replay 拒绝时刻的
    // 读数，而该读数必须晚于 update 原子切换的时间戳。
    let refused = runtime
        .uninstall_application_gated_by_task_activity(PackageId::from_bytes(PACKAGE), 0x14)
        .expect_err("the W27-D gate must refuse while the task is outstanding");
    assert!(
        matches!(
            &refused,
            SliceKError::Application(ApplicationAuthorityError::ApplicationActiveTasksRunning {
                active_task_count: 1,
                ..
            })
        ),
        "unexpected refusal: {refused:?}"
    );

    // ---- 4. update (compatibility window + migration runner) ----
    let cross_major = runtime
        .applications
        .migrate_application(
            &runtime.artifacts,
            MigrateApplicationRequest {
                package_id: PackageId::from_bytes(PACKAGE),
                package_verification_receipt_id: v2.receipt_id,
                idempotency_key: k(0x4C),
                compatibility_window: CompatibilityWindow::SameMajor,
                declared_step_count: 2,
                requested_at_ms: runtime.wall_now_ms(k(0x63)).expect("cross-major clock"),
            },
        )
        .expect_err("cross-major target must violate the SameMajor window");
    assert!(
        matches!(
            cross_major,
            ApplicationAuthorityError::UpdateCompatibilityViolation { .. }
        ),
        "unexpected refusal: {cross_major:?}"
    );

    let drill = k(0x48);
    let view = match runtime
        .applications
        .migrate_application(
            &runtime.artifacts,
            MigrateApplicationRequest {
                package_id: PackageId::from_bytes(PACKAGE),
                package_verification_receipt_id: v11.receipt_id,
                idempotency_key: drill,
                compatibility_window: CompatibilityWindow::SameMajor,
                declared_step_count: 2,
                requested_at_ms: runtime.wall_now_ms(k(0x5E)).expect("begin clock"),
            },
        )
        .expect("begin migration")
    {
        MigrateDecision::Started(view) => view,
        MigrateDecision::Replayed(view) => panic!("fresh drill cannot replay: {view:?}"),
    };
    assert_eq!(view.state, MigrationState::Pending);
    assert_eq!(view.from_generation.get(), 1);
    assert_eq!(view.target_package_version, v11.package_version);

    for (step, clock_tag) in [(1_u64, 0x4D_u8), (2, 0x4F)] {
        match runtime
            .applications
            .record_migration_step(RecordMigrationStepRequest {
                idempotency_key: drill,
                step_index: step,
                completed_at_ms: runtime.wall_now_ms(k(clock_tag)).expect("step clock"),
            })
            .expect("record step")
        {
            RecordMigrationStepDecision::Recorded(record) => assert_eq!(record.step_index, step),
            RecordMigrationStepDecision::Replayed(record) => {
                panic!("fresh step cannot replay: {record:?}")
            }
        }
    }

    let probe = CountingProbe {
        calls: Cell::new(0),
        seen_steps: Cell::new((0, 0)),
    };
    let verdict = runtime
        .applications
        .run_migration_health_check(
            &runtime.artifacts,
            drill,
            &probe,
            runtime.wall_now_ms(k(0x5F)).expect("health clock"),
        )
        .expect("health check");
    match &verdict {
        MigrationHealthDecision::Recorded(report) => assert!(report.passed),
        MigrationHealthDecision::Replayed(report) => {
            panic!("fresh verdict cannot replay: {report:?}")
        }
    }
    assert_eq!(probe.calls.get(), 1, "the probe is consulted exactly once");
    assert_eq!(probe.seen_steps.get(), (2, 2));

    let installation_v11 = match runtime
        .applications
        .activate_package_migration(
            &runtime.artifacts,
            ActivatePackageMigrationRequest {
                idempotency_key: drill,
                activated_at_ms: runtime.wall_now_ms(k(0x6A)).expect("activation clock"),
            },
        )
        .expect("activate migration")
    {
        ActivateMigrationDecision::Activated(receipt) => receipt,
        ActivateMigrationDecision::Replayed(receipt) => {
            panic!("fresh activation cannot replay: {receipt:?}")
        }
    };
    assert_eq!(installation_v11.installation_generation.get(), 2);
    assert_eq!(
        installation_v11.package_manifest_digest,
        v11.manifest_digest
    );
    assert_eq!(
        probe.calls.get(),
        1,
        "activation never re-consults the probe"
    );
    let history = runtime
        .applications
        .list_installations(application_id)
        .expect("installation history");
    assert_eq!(history.len(), 2, "dense generation history 1,2");
    assert_eq!(history[0].installation_generation.get(), 1);
    assert_eq!(history[1].installation_generation.get(), 2);
    let application = runtime
        .applications
        .inspect_application(PackageId::from_bytes(PACKAGE))
        .expect("inspect")
        .expect("application");
    assert_eq!(application.status, ApplicationStatus::Installed);
    assert_eq!(application.current_installation_generation.get(), 2);
    assert_eq!(application.package_manifest_digest, v11.manifest_digest);

    // ---- 5. uninstall (W30-D teardown chain + gated uninstall) ----
    let registry = SupervisorPidRegistry::new();
    let service_pid = match service_child.as_deref_mut() {
        Some(child) => child.id(),
        None => std::process::id(),
    };
    registry
        .register(RegisterSupervisorPidRequest {
            process_id: process.process_id,
            process_generation: process.process_generation,
            os_pid: service_pid,
            registered_at_ms: runtime.wall_now_ms(k(0x5C)).expect("supervisor clock"),
        })
        .expect("register supervisor pid");

    let teardown = run_application_teardown(
        &runtime,
        &tokio_adapter,
        PackageId::from_bytes(PACKAGE),
        0x12,
        &registry,
    )
    .expect("application teardown");
    assert_eq!(teardown.application_id, application_id);
    assert_eq!(teardown.kills.len(), 1);
    assert_eq!(teardown.crashes.len(), 1);
    assert_eq!(teardown.linkages.len(), 1);
    assert!(matches!(
        teardown.kills[0],
        PlatformKillDecision::Signaled(_)
    ));
    assert_eq!(
        teardown.crashes[0].lifecycle_state,
        ProcessLifecycleState::Crashed
    );
    // 匹配到的执行体恰是 run 阶段已完成的写 fiber（already_terminal），
    // 而崩溃批处理仍然围栏其取消作用域（canceled_scopes）。
    assert_eq!(teardown.linkages[0].matched_fibers, 1);
    assert_eq!(teardown.linkages[0].already_terminal, 1);
    assert_eq!(teardown.linkages[0].canceled_scopes, 1);
    assert_eq!(teardown.linkages[0].vanished_scopes, 0);
    let CancelDecision::Applied { cancel_epoch, .. } = &teardown.task_cancels[0] else {
        panic!(
            "fresh teardown cancel must apply: {:?}",
            teardown.task_cancels[0]
        )
    };
    assert_eq!(*cancel_epoch, 1);
    let task_row = runtime.tasks.inspect_task(task_id).expect("task row");
    assert_eq!(task_row.state, nlos_task::TaskState::Cancelled);
    let application = runtime
        .applications
        .inspect_application(PackageId::from_bytes(PACKAGE))
        .expect("inspect")
        .expect("application");
    assert_eq!(application.status, ApplicationStatus::Uninstalled);
    assert!(
        matches!(
            runtime
                .process
                .inspect_active_process_binding(process.process_id),
            Err(nlos_process::ProcessAuthorityError::ProcessBindingTerminal(
                ProcessLifecycleState::Crashed
            ))
        ),
        "the binding is terminal after the kill chain"
    );
    // A fresh fiber under the killed process's scope refuses admission.
    assert!(matches!(
        tokio_adapter.spawn_fiber(
            FiberSpec {
                fiber_id: nlos_types::ExecutionFiberId::from_bytes(id(0x67)),
                fiber_generation: Generation::INITIAL,
                agent_instance_id: process.agent_instance_id,
                agent_generation: process.agent_instance_generation,
                process_id: process.process_id,
                process_generation: process.process_generation,
                task_attempt_id: Some(attempt_id),
                cancellation_scope_id: scope_id,
                cancellation_generation: Generation::INITIAL,
                resource_group_id: ResourceGroupId::from_bytes(id(0x68)),
                scheduler_domain_id: SchedulerDomainId::from_bytes(id(0x69)),
                deadline: None,
            },
            Box::pin(pending()),
        ),
        Err(RuntimeError::Cancelled)
    ));

    // Replay: the whole teardown re-run through the original registry
    // replays byte-identical receipts while the kill replay STILL drives
    // the adapter (at-least-once) — the killed service stand-in is an
    // unreaped zombie, so the supplementary SIGTERM is a no-op success.
    let replay = run_application_teardown(
        &runtime,
        &tokio_adapter,
        PackageId::from_bytes(PACKAGE),
        0x12,
        &registry,
    )
    .expect("teardown replay");
    assert!(matches!(replay.kills[0], PlatformKillDecision::Replayed(_)));
    assert_eq!(replay.crashes, teardown.crashes);
    assert_eq!(
        replay.task_cancels[0],
        CancelDecision::Replayed { cancel_epoch: 1 }
    );
    assert_eq!(replay.uninstall, teardown.uninstall);

    if let Some(child) = service_child {
        let status = child.wait().expect("wait for killed service child");
        assert!(!status.success(), "the service stand-in died by signal");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn third_party_sample_lifecycle_full_chain_headless() {
    let dir = TempDir::new("lifecycle");
    let mut child = std::process::Command::new("sleep")
        .arg("600")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn service stand-in");
    third_party_sample_lifecycle_body(&dir, Some(&mut child)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(not(unix))]
async fn third_party_sample_lifecycle_contract_via_noop_adapter() {
    let dir = TempDir::new("lifecycle-contract");
    third_party_sample_lifecycle_body(&dir, None).await;
}
