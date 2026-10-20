//! W60 (F9 后半/车道③): the install→plan head segment — "装包即提案计划"
//! end to end. The templated install face
//! ([`SliceKRuntime::install_verified_templated_package`]) binds the
//! application authority's install commit to the plan authority's
//! declaration face: the signed `tasks` segment compiles
//! (`compile_task_templates`, `[PLAN-OVERRIDE-001]`) and applies as one
//! plan revision behind the task authority's declaration-admission
//! consult, under the `(application_id, installation_generation)`
//! domain-separated idempotency key.
//!
//! Covered, one test per contract clause:
//!
//! - install of a templated package → the plan authority carries the
//!   declared revision and nodes (kind + dependency shape asserted from
//!   the durable resolution receipt, not the inputs);
//! - same-generation replay → install `Replayed` AND plan `Replayed`,
//!   zero new revisions;
//! - update (new installation generation) → a new **total revision of
//!   the same plan** (digest chain, declared-revision advance);
//! - a package without a `tasks` segment (the legacy face) → no plan
//!   revision at all;
//! - receipt↔package pairing mismatch → typed refusal before any
//!   durable write;
//! - apply-time admission denial (a segment projecting past the Task
//!   tier's `max_task_nodes`) → typed plan error, install fact retained,
//!   replay still re-attempts the application;
//! - an idempotency conflict injected at the derived key of the upcoming
//!   generation → the honest window (install committed, revision
//!   missing) and convergence by replaying the installs after the plan
//!   store is repaired;
//! - the daemon's materialization driver posture: `scheduler.select` +
//!   `drive` over the installed plan opens the W31-A gate for the
//!   dependency-free node and the dependency edge keeps the dependent
//!   skipped until the dependency completes.

use ed25519_dalek::Signer;
use nlos_artifact::{
    ContentDigest, CreateArtifactSpec, PackageEntryRole, PackageManifest, PackageManifestEntry,
    PackageTaskKind, PackageTaskTemplate, ProvenanceSourceTriple, PutRevisionRequest,
    SignedPackageWithTasks, VerifyPackageWithTasksRequest, package_manifest_with_tasks_message,
};
use nlos_plan::{
    AdmissionConsult, AdmissionConsultOutcome, ApplyPlanRevisionRequest, MaterializationAdmission,
    MaterializationRejection, MaterializationScheduler, PlanNodeKind, PlanNodeState,
    PlanResolutionDecision, PlanRevisionDecision, PlanRevisionSelector, PlanStoreError,
    ResolvePlanRequest,
};
use nlos_slice_k::{
    SliceKError, SliceKRuntime, TemplatedInstallReceipt, install_plan_revision_key, seeded_key,
};
use nlos_task::{MaterializationAdmissionFacts, SqliteTaskAuthority, TaskStoreError};
use nlos_types::{ApplicationId, Generation, PackageId, TaskPlanId};

/// The fixture package identity (one application per test scenario).
const PACKAGE: [u8; 16] = [
    0x77, 0x0a, 0x2f, 0x61, 0xc4, 0x19, 0xbb, 0x3d, 0x8e, 0x50, 0xf7, 0x26, 0x01, 0x64, 0xd2, 0x99,
];
const NODE_MAIN: [u8; 16] = [
    0x01, 0xaa, 0x3c, 0x57, 0xb9, 0xd1, 0xe2, 0xf4, 0xa6, 0xb8, 0xc0, 0xd2, 0xe4, 0xf6, 0xa8, 0xb0,
];
const NODE_SERVICE: [u8; 16] = [
    0x02, 0xce, 0x7a, 0x19, 0xd5, 0xf3, 0xb1, 0xa9, 0xc7, 0xe5, 0xd3, 0xf1, 0xb9, 0xa7, 0xc5, 0xe3,
];

struct TempDir {
    root: std::path::PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-slice-k-install-plan-{name}-{}-{sequence}",
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
            Err(error) => panic!("remove install-plan temp root: {error}"),
        }
    }
}

/// One version's two-template segment: `main` (agent role) and `service`
/// (executable) depending on `main`; the version tag varies the binding
/// digests so distinct versions declare distinct shapes.
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
            input_selectors_digest: ContentDigest::of_bytes(b"main-inputs").into_bytes(),
            output_contract_digest: ContentDigest::of_bytes(b"main-outputs").into_bytes(),
            policy_digest: ContentDigest::of_bytes(b"main-policy").into_bytes(),
            resource_ceiling_digest: ContentDigest::of_bytes(b"main-ceiling").into_bytes(),
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

/// One built, signed, and verified templated package revision (payload
/// entry + `tasks` segment), riding the same key bands as
/// [`SliceKRuntime::publish_signed_package`] under this scenario's own
/// seed.
struct VerifiedTemplated {
    receipt: nlos_artifact::PackageVerificationReceipt,
    signed: SignedPackageWithTasks,
}

fn verified_templated(
    runtime: &SliceKRuntime,
    publisher: &nlos_slice_k::Publisher,
    seed: u8,
    version: u64,
    version_tag: &str,
    tasks: Vec<PackageTaskTemplate>,
) -> VerifiedTemplated {
    let payload = format!("install-plan payload {version_tag}\n").into_bytes();
    let artifact_id = nlos_types::ArtifactId::from_bytes([seed.wrapping_add(10); 16]);
    let at_ms = runtime
        .wall_now_ms(seeded_key(seed, 12))
        .expect("publish clock");
    runtime
        .artifacts
        .create_artifact(CreateArtifactSpec {
            artifact_id,
            idempotency_key: seeded_key(seed, 11),
            content_type: "application/octet-stream".to_string(),
            application_id: None,
            owner: None,
            created_at_ms: at_ms,
        })
        .expect("create payload artifact");
    runtime
        .artifacts
        .put_revision(PutRevisionRequest {
            artifact_id,
            expected_head_revision: 0,
            bytes: &payload,
            created_at_ms: at_ms,
            provenance: ProvenanceSourceTriple {
                source_a: [seed; 16],
                source_b: PACKAGE,
                source_digest: ContentDigest::of_bytes(&payload),
            },
        })
        .expect("put payload revision");
    let manifest = PackageManifest {
        package_id: PackageId::from_bytes(PACKAGE),
        version,
        entries: vec![PackageManifestEntry {
            name: "payload".to_string(),
            artifact_id,
            digest: ContentDigest::of_bytes(&payload),
            role: PackageEntryRole::Data,
        }],
    };
    let message = package_manifest_with_tasks_message(&manifest, &tasks);
    let signed = SignedPackageWithTasks {
        manifest,
        tasks,
        signer: publisher.principal_id,
        signature: publisher.signing.sign(&message).to_bytes(),
    };
    let decision = runtime
        .artifacts
        .verify_package_with_tasks(
            &runtime.identity,
            VerifyPackageWithTasksRequest {
                signed: &signed,
                idempotency_key: seeded_key(seed, 14),
                verified_at_ms: runtime
                    .wall_now_ms(seeded_key(seed, 13))
                    .expect("verify clock"),
            },
        )
        .expect("verify templated package");
    VerifiedTemplated {
        receipt: decision.receipt().clone(),
        signed,
    }
}

/// Installs `v1` (two templates) once and returns the runtime facts the
/// scenario tests share: the runtime, the application id, and the receipt.
fn installed_v1(
    name: &str,
) -> (
    TempDir,
    SliceKRuntime,
    ApplicationId,
    TemplatedInstallReceipt,
) {
    let dir = TempDir::new(name);
    let runtime = SliceKRuntime::open(dir.root()).expect("open slice-k runtime");
    let publisher = runtime.bootstrap_publisher(0xA0).expect("publisher");
    let v1 = verified_templated(&runtime, &publisher, 0xB1, 1, "v1.0.0", templates("v1.0.0"));
    let install = runtime
        .install_verified_templated_package(&v1.receipt, &v1.signed, 0x10)
        .expect("templated install");
    assert_eq!(install.installation.installation_generation.get(), 1);
    (dir, runtime, install.installation.application_id, install)
}

/// The daemon-posture Worker-tier consult (the W31-A twin of
/// `nlos-system-control`'s `TaskAuthorityMaterializationConsult`), test
/// local so the scheduler battery rides the same production shape.
struct TaskMaterializationConsult<'a> {
    tasks: &'a SqliteTaskAuthority,
}

impl AdmissionConsult for TaskMaterializationConsult<'_> {
    type Error = TaskStoreError;

    fn consult_materialization(
        &self,
        other_declared_task_nodes: u64,
    ) -> Result<AdmissionConsultOutcome, TaskStoreError> {
        match self
            .tasks
            .answer_plan_materialization(other_declared_task_nodes)
        {
            Ok(MaterializationAdmissionFacts {
                profile_id,
                projected_task_nodes,
                projected_active_working_set,
            }) => Ok(AdmissionConsultOutcome::Admitted(
                MaterializationAdmission {
                    profile_id: profile_id.to_string(),
                    projected_task_nodes,
                    projected_active_working_set,
                },
            )),
            Err(TaskStoreError::WorkingSetAdmissionDenied {
                profile_id,
                active_count,
                max_active_working_set,
            }) => Ok(AdmissionConsultOutcome::Denied(
                MaterializationRejection::WorkingSetFull {
                    profile_id: profile_id.to_string(),
                    active_count,
                    max_active_working_set,
                },
            )),
            Err(TaskStoreError::TaskNodeAdmissionDenied {
                profile_id,
                task_count,
                max_task_nodes,
            }) => Ok(AdmissionConsultOutcome::Denied(
                MaterializationRejection::TaskNodeCapExceeded {
                    profile_id: profile_id.to_string(),
                    task_count,
                    max_task_nodes,
                },
            )),
            Err(other) => Err(other),
        }
    }
}

#[test]
fn templated_install_applies_plan_revision_with_declared_shape() {
    let (_dir, runtime, application_id, install) = installed_v1("declared-shape");

    let PlanRevisionDecision::Applied(receipt) = &install.plan else {
        panic!("fresh install must apply, not replay: {:?}", install.plan);
    };
    // The revision key is the domain-separated (application, generation)
    // derivation — not any seed.
    assert_eq!(
        receipt.idempotency_key,
        install_plan_revision_key(application_id, Generation::INITIAL)
    );
    assert_eq!(receipt.declared_node_count, 2);
    assert_eq!(receipt.parent_revision_digest, None);

    let plans = runtime.plans().list_plan_ids().expect("list plans");
    assert_eq!(plans, vec![receipt.plan_id], "exactly one plan");
    let view = runtime
        .plans()
        .inspect_plan(receipt.plan_id)
        .expect("inspect plan")
        .expect("plan exists");
    assert_eq!(view.current_revision, 1);

    // Node metadata carries the declared kinds under the declared keys.
    let nodes = runtime
        .plans()
        .list_plan_nodes(receipt.plan_id)
        .expect("list plan nodes");
    assert_eq!(nodes.len(), 2);
    for node in &nodes {
        assert_eq!(node.state, PlanNodeState::Declared);
        assert_eq!(node.declared_revision, 1);
        assert_eq!(node.plan_id, receipt.plan_id);
    }
    let kind_of = |key: [u8; 16]| {
        nodes
            .iter()
            .find(|node| node.node_key == key)
            .unwrap_or_else(|| panic!("node {key:02x?} missing"))
            .kind
    };
    assert_eq!(kind_of(NODE_MAIN), PlanNodeKind::AgentRole);
    assert_eq!(kind_of(NODE_SERVICE), PlanNodeKind::Executable);

    // Dependency shape from the durable resolution receipt: `service`
    // depends on `main`, and the resolved topological order is
    // dependencies-first.
    let resolution = match runtime
        .plans()
        .resolve_plan(ResolvePlanRequest {
            selector: PlanRevisionSelector::Current(receipt.plan_id),
            idempotency_key: seeded_key(0xE1, 1),
            resolved_at_ms: runtime
                .wall_now_ms(seeded_key(0xE1, 2))
                .expect("resolve clock"),
        })
        .expect("resolve plan")
    {
        PlanResolutionDecision::Resolved(handle) => handle,
        PlanResolutionDecision::Replayed(handle) => {
            panic!("fresh resolution must resolve, not replay: {handle:?}")
        }
    };
    assert_eq!(resolution.revision, 1);
    let node_id_of = |key: [u8; 16]| {
        nodes
            .iter()
            .find(|node| node.node_key == key)
            .unwrap_or_else(|| panic!("node {key:02x?} missing"))
            .node_id
    };
    let (main_id, service_id) = (node_id_of(NODE_MAIN), node_id_of(NODE_SERVICE));
    assert_eq!(resolution.resolved_order, vec![main_id, service_id]);
    assert_eq!(resolution.resolved_edges, vec![(service_id, main_id)]);
}

#[test]
fn same_generation_replay_applies_no_new_revision() {
    let (_dir, runtime, _application_id, first) = installed_v1("same-gen-replay");
    let plan_id = first.plan_receipt().plan_id;

    // The identical call (same seed → same install key, same derived plan
    // key, same durable clock reading) replays both authorities.
    let publisher = runtime.bootstrap_publisher(0xA0).expect("publisher");
    let again = verified_templated(&runtime, &publisher, 0xB1, 1, "v1.0.0", templates("v1.0.0"));
    let replay = runtime
        .install_verified_templated_package(&again.receipt, &again.signed, 0x10)
        .expect("replay templated install");
    assert_eq!(
        replay.installation, first.installation,
        "the application authority replays the original receipt"
    );
    assert!(
        matches!(replay.plan, PlanRevisionDecision::Replayed(_)),
        "plan must replay, not apply: {:?}",
        replay.plan
    );
    assert_eq!(replay.plan_receipt(), first.plan_receipt());

    let view = runtime
        .plans()
        .inspect_plan(plan_id)
        .expect("inspect plan")
        .expect("plan exists");
    assert_eq!(view.current_revision, 1, "no second revision");
    assert!(
        runtime
            .plans()
            .inspect_plan_revision(plan_id, 2)
            .expect("inspect revision 2")
            .is_none()
    );
    assert_eq!(
        runtime.plans().list_plan_ids().expect("list plans"),
        vec![plan_id]
    );
}

#[test]
fn update_generation_applies_new_total_revision_of_same_plan() {
    let (_dir, runtime, application_id, first) = installed_v1("update-generation");
    let plan_id = first.plan_receipt().plan_id;
    let revision1_digest = first.plan_receipt().plan_digest;

    // A newer verified package of the same identity; a fresh install seed
    // is a new exactly-once install → the generation advances to 2.
    let publisher = runtime.bootstrap_publisher(0xA0).expect("publisher");
    let v2 = verified_templated(&runtime, &publisher, 0xB2, 2, "v2.0.0", templates("v2.0.0"));
    let update = runtime
        .install_verified_templated_package(&v2.receipt, &v2.signed, 0x11)
        .expect("update install");
    assert_eq!(update.installation.installation_generation.get(), 2);

    let PlanRevisionDecision::Applied(receipt) = &update.plan else {
        panic!(
            "new generation must apply a new revision: {:?}",
            update.plan
        );
    };
    // Total-revision semantics: SAME plan, chained parent digest, the
    // derived key of generation 2.
    assert_eq!(receipt.plan_id, plan_id, "update revises the same plan");
    assert_eq!(receipt.revision, 2);
    assert_eq!(
        receipt.parent_revision_digest,
        Some(revision1_digest),
        "revision 2 chains onto revision 1's digest"
    );
    assert_eq!(
        receipt.idempotency_key,
        install_plan_revision_key(application_id, update.installation.installation_generation)
    );

    // Pre-execution nodes advanced to the new declared revision; the
    // declared identities (keys, kinds) survive the total re-declaration.
    let nodes = runtime
        .plans()
        .list_plan_nodes(plan_id)
        .expect("list plan nodes");
    assert_eq!(nodes.len(), 2);
    assert!(nodes.iter().all(|node| node.declared_revision == 2));
    assert_eq!(
        nodes
            .iter()
            .find(|node| node.node_key == NODE_MAIN)
            .expect("main node")
            .kind,
        PlanNodeKind::AgentRole
    );

    // Replaying the update changes nothing.
    let replay = runtime
        .install_verified_templated_package(&v2.receipt, &v2.signed, 0x11)
        .expect("update replay");
    assert_eq!(replay.installation, update.installation);
    assert!(matches!(replay.plan, PlanRevisionDecision::Replayed(_)));
    assert_eq!(
        runtime
            .plans()
            .inspect_plan(plan_id)
            .expect("inspect plan")
            .expect("plan exists")
            .current_revision,
        2
    );
}

#[test]
fn legacy_install_without_templates_applies_no_plan_revision() {
    let dir = TempDir::new("legacy-no-templates");
    let runtime = SliceKRuntime::open(dir.root()).expect("open slice-k runtime");
    let publisher = runtime.bootstrap_publisher(0xA1).expect("publisher");
    let package = runtime
        .publish_signed_package(&publisher, 0xB3, b"legacy payload")
        .expect("publish legacy package");
    let verification = runtime
        .verify_signed_package(&package, 0xB3)
        .expect("verify legacy package");
    runtime
        .install_verified_package(&verification, 0xB3)
        .expect("legacy install");

    // No `tasks` segment ⇒ nothing to declare ⇒ the plan authority stays
    // empty (the legacy face has no proposal to apply).
    assert_eq!(
        runtime.plans().list_plan_ids().expect("list plans"),
        Vec::<TaskPlanId>::new()
    );
    assert!(
        runtime
            .applications
            .inspect_application(package.package_id)
            .expect("inspect application")
            .is_some(),
        "the legacy install itself succeeded"
    );
}

#[test]
fn receipt_package_pairing_mismatch_refuses_before_any_durable_state() {
    let dir = TempDir::new("pairing-mismatch");
    let runtime = SliceKRuntime::open(dir.root()).expect("open slice-k runtime");
    let publisher = runtime.bootstrap_publisher(0xA0).expect("publisher");
    let v1 = verified_templated(&runtime, &publisher, 0xB1, 1, "v1.0.0", templates("v1.0.0"));
    let v2 = verified_templated(&runtime, &publisher, 0xB2, 2, "v2.0.0", templates("v2.0.0"));

    // v1's receipt presented with v2's signed package: the pairing check
    // refuses before the install authority call — zero durable install
    // or plan state.
    let refused = runtime
        .install_verified_templated_package(&v1.receipt, &v2.signed, 0x10)
        .expect_err("mismatched pairing must refuse");
    assert!(
        matches!(refused, SliceKError::InstallPlanState(_)),
        "unexpected refusal: {refused}"
    );
    assert!(
        runtime
            .applications
            .inspect_application(PackageId::from_bytes(PACKAGE))
            .expect("inspect application")
            .is_none(),
        "no application may exist after the pairing refusal"
    );
    assert_eq!(
        runtime.plans().list_plan_ids().expect("list plans"),
        Vec::<TaskPlanId>::new()
    );
}

/// The Task tier's default profile caps `max_task_nodes` at `10_000`; a
/// manifest may declare up to `100_000` templates, so a 10_004-template
/// segment verifies but its projection is past the cap — the production
/// consult's typed denial, reached end to end.
#[test]
fn admission_denial_keeps_install_fact_and_replay_keeps_attempting() {
    let dir = TempDir::new("admission-denial");
    let runtime = SliceKRuntime::open(dir.root()).expect("open slice-k runtime");
    let publisher = runtime.bootstrap_publisher(0xA0).expect("publisher");
    let mut wide = Vec::new();
    for index in 0_u64..10_004 {
        let mut node_key = [0_u8; 16];
        node_key[..8].copy_from_slice(&index.to_be_bytes());
        wide.push(PackageTaskTemplate {
            node_key,
            kind: PackageTaskKind::Executable,
            binding_digest: ContentDigest::of_bytes(&index.to_be_bytes()).into_bytes(),
            dependency_keys: Vec::new(),
            input_selectors_digest: [1; 32],
            output_contract_digest: [2; 32],
            policy_digest: [3; 32],
            resource_ceiling_digest: [4; 32],
        });
    }
    let wide_package = verified_templated(&runtime, &publisher, 0xB4, 1, "wide", wide);

    // The install commits (application authority), then the plan
    // application is denied by the Task-tier consult: the typed window —
    // install fact retained, revision not applied.
    let denied = runtime
        .install_verified_templated_package(&wide_package.receipt, &wide_package.signed, 0x10)
        .expect_err("the over-cap projection must be denied");
    match &denied {
        SliceKError::Plan(PlanStoreError::DeclarationAdmissionDenied {
            projected_task_nodes,
            max_task_nodes,
            ..
        }) => {
            assert_eq!(*projected_task_nodes, 10_004);
            assert_eq!(*max_task_nodes, 10_000);
        }
        other => panic!("unexpected refusal: {other:?}"),
    }
    let application = runtime
        .applications
        .inspect_application(PackageId::from_bytes(PACKAGE))
        .expect("inspect application")
        .expect("the install fact is retained");
    assert_eq!(application.current_installation_generation.get(), 1);
    assert_eq!(
        runtime
            .applications
            .list_installations(application.application_id)
            .expect("installation history")
            .len(),
        1,
        "exactly one installation receipt — the denial added none"
    );
    assert_eq!(
        runtime.plans().list_plan_ids().expect("list plans"),
        Vec::<TaskPlanId>::new(),
        "a denied declaration writes no revision"
    );

    // Replay: the install replays its receipt AND the head segment still
    // attempts the plan application — the deterministic denial recurring
    // after `Replayed` is the observable proof of the re-attempt (a skip
    // would return a receipt, not the typed refusal).
    let denied_again = runtime
        .install_verified_templated_package(&wide_package.receipt, &wide_package.signed, 0x10)
        .expect_err("replay must re-attempt the denied application");
    assert!(
        matches!(
            &denied_again,
            SliceKError::Plan(PlanStoreError::DeclarationAdmissionDenied { .. })
        ),
        "unexpected replay refusal: {denied_again}"
    );
    assert_eq!(
        runtime
            .applications
            .inspect_application(PackageId::from_bytes(PACKAGE))
            .expect("inspect application")
            .expect("installed")
            .current_installation_generation
            .get(),
        1,
        "the replay advanced nothing"
    );
}

/// One linear window→repair→replay scenario; splitting it would hide the
/// causality the assertions trace.
#[allow(clippy::too_many_lines)]
#[test]
fn plan_conflict_window_keeps_install_fact_and_replay_converges() {
    let (dir, runtime, application_id, first) = installed_v1("conflict-window");
    let plan_id = first.plan_receipt().plan_id;
    let publisher = runtime.bootstrap_publisher(0xA0).expect("publisher");
    let v2 = verified_templated(&runtime, &publisher, 0xB2, 2, "v2.0.0", templates("v2.0.0"));

    // Inject a conflicting revision under the derived key of the UPCOMING
    // generation (the fixture-only ungated apply): when the update
    // install commits and its head segment derives the same key, the
    // plan authority refuses the content rebinding.
    let conflict_key = install_plan_revision_key(
        application_id,
        Generation::new(std::num::NonZeroU64::new(2).expect("nonzero")),
    );
    let poisoned = runtime
        .plans()
        .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
            plan_id: Some(plan_id),
            nodes: vec![nlos_plan::PlanNodeDeclaration {
                node_key: NODE_MAIN,
                kind: PlanNodeKind::AgentRole,
                binding_digest: [0xEE; 32],
                dependency_keys: Vec::new(),
                input_selectors_digest: [1; 32],
                output_contract_digest: [2; 32],
                policy_digest: [3; 32],
                resource_ceiling_digest: [4; 32],
                conditions: None,
            }],
            idempotency_key: conflict_key,
            applied_at_ms: runtime
                .wall_now_ms(seeded_key(0xE2, 1))
                .expect("poison clock"),
        })
        .expect("inject conflicting revision");
    assert_eq!(poisoned.receipt().revision, 2);

    // The update install commits generation 2; the plan application hits
    // the poisoned key and fails closed typed — the honest window.
    let conflicted = runtime
        .install_verified_templated_package(&v2.receipt, &v2.signed, 0x11)
        .expect_err("the poisoned key must refuse the plan application");
    assert!(
        matches!(
            &conflicted,
            SliceKError::Plan(PlanStoreError::IdempotencyConflict)
        ),
        "unexpected refusal: {conflicted}"
    );
    let application = runtime
        .applications
        .inspect_application(PackageId::from_bytes(PACKAGE))
        .expect("inspect application")
        .expect("installed");
    assert_eq!(application.current_installation_generation.get(), 2);
    assert_eq!(
        runtime
            .applications
            .list_installations(application_id)
            .expect("installation history")
            .len(),
        2,
        "the generation-2 installation fact is retained"
    );

    // Convergence: the plan store is repaired (restored to a pre-poison
    // state — the operator action this test stands in for), then BOTH
    // installs replay in generation order: the generation-1 replay
    // recreates the plan genesis (install `Replayed`, plan `Applied`),
    // the generation-2 replay lands revision 2 under its derived key.
    drop(runtime);
    for suffix in ["plans.sqlite3", "plans.sqlite3-wal", "plans.sqlite3-shm"] {
        let path = dir.root().join(suffix);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove {suffix}: {error}"),
        }
    }
    let runtime = SliceKRuntime::open(dir.root()).expect("reopen slice-k runtime");
    let publisher = runtime.bootstrap_publisher(0xA0).expect("publisher");
    let v1_again = verified_templated(&runtime, &publisher, 0xB1, 1, "v1.0.0", templates("v1.0.0"));
    let replay_v1 = runtime
        .install_verified_templated_package(&v1_again.receipt, &v1_again.signed, 0x10)
        .expect("generation-1 replay converges");
    assert_eq!(replay_v1.installation.installation_generation.get(), 1);
    assert!(
        matches!(replay_v1.plan, PlanRevisionDecision::Applied(_)),
        "the plan application runs on the Replayed install: {:?}",
        replay_v1.plan
    );
    let converged_plan = replay_v1.plan_receipt().plan_id;
    assert_eq!(
        converged_plan, plan_id,
        "the recreated plan derives the same TaskPlanId"
    );

    let replay_v2 = runtime
        .install_verified_templated_package(&v2.receipt, &v2.signed, 0x11)
        .expect("generation-2 replay converges");
    assert_eq!(replay_v2.installation.installation_generation.get(), 2);
    let PlanRevisionDecision::Applied(converged) = &replay_v2.plan else {
        panic!("converged update must apply: {:?}", replay_v2.plan);
    };
    assert_eq!(converged.revision, 2);
    assert_eq!(converged.plan_id, plan_id);
    assert_eq!(
        converged.idempotency_key, conflict_key,
        "the derived generation-2 key names the converged revision"
    );
}

#[test]
fn scheduler_select_and_drive_opens_the_materialization_gate() {
    let (_dir, runtime, _application_id, install) = installed_v1("scheduler-gate");
    let plan_id = install.plan_receipt().plan_id;
    let nodes = runtime
        .plans()
        .list_plan_nodes(plan_id)
        .expect("list plan nodes");
    let node_id_of = |key: [u8; 16]| {
        nodes
            .iter()
            .find(|node| node.node_key == key)
            .unwrap_or_else(|| panic!("node {key:02x?} missing"))
            .node_id
    };
    let (main_id, service_id) = (node_id_of(NODE_MAIN), node_id_of(NODE_SERVICE));

    // One driver pass (the daemon's materialization driver runs exactly
    // this posture over `list_plan_ids`): the dependency-free node is
    // selected for a fresh gate round; the dependent is skipped with the
    // durable dependency edge named — the declared shape driving the
    // gate, not any installer-side hint.
    let consult = TaskMaterializationConsult {
        tasks: &runtime.tasks,
    };
    let mut scheduler = MaterializationScheduler::new(4);
    let report = scheduler.select(runtime.plans(), plan_id).expect("select");
    assert_eq!(
        report
            .selections
            .iter()
            .map(|entry| (entry.node_id, entry.kind))
            .collect::<Vec<_>>(),
        vec![(main_id, nlos_plan::SelectionKind::NewGateRound)]
    );
    assert_eq!(report.skips.len(), 1);
    assert_eq!(
        report.skips[0].reason,
        nlos_plan::SelectionSkipReason::DependenciesNotReady {
            unresolved: vec![main_id]
        }
    );

    let at_ms = runtime
        .wall_now_ms(seeded_key(0xF0, 1))
        .expect("drive clock");
    let summary = scheduler
        .drive(runtime.plans(), &report, &consult, at_ms)
        .expect("drive");
    assert_eq!(summary.approved, 1);
    assert_eq!(summary.rejected, 0);
    let main_state = runtime
        .plans()
        .inspect_node(plan_id, main_id)
        .expect("inspect main node")
        .expect("main node exists")
        .state;
    assert_eq!(main_state, PlanNodeState::Materializing);

    // Manual resolve: walk the node to COMPLETED (two legal §25.2.1
    // edges under fresh keys), then the next pass selects the dependent.
    let transition = |from, to, offset| {
        let now = runtime
            .wall_now_ms(seeded_key(0xF0, offset))
            .expect("transition clock");
        runtime
            .plans()
            .record_node_transition(nlos_plan::NodeTransitionRequest {
                plan_id,
                node_id: main_id,
                from_state: from,
                to_state: to,
                expected_declared_revision: 1,
                idempotency_key: seeded_key(0xF0, offset),
                transitioned_at_ms: now,
            })
            .expect("record transition");
    };
    transition(PlanNodeState::Materializing, PlanNodeState::Active, 2);
    transition(PlanNodeState::Active, PlanNodeState::Completed, 3);

    let report2 = scheduler
        .select(runtime.plans(), plan_id)
        .expect("select 2");
    assert_eq!(
        report2
            .selections
            .iter()
            .map(|entry| entry.node_id)
            .collect::<Vec<_>>(),
        vec![service_id],
        "the completed dependency unblocks the dependent: {:?}",
        report2.skips
    );
    let at_ms = runtime
        .wall_now_ms(seeded_key(0xF0, 4))
        .expect("drive clock 2");
    let summary2 = scheduler
        .drive(runtime.plans(), &report2, &consult, at_ms)
        .expect("drive 2");
    assert_eq!(summary2.approved, 1);
    assert_eq!(
        runtime
            .plans()
            .inspect_node(plan_id, service_id)
            .expect("inspect service node")
            .expect("service node exists")
            .state,
        PlanNodeState::Materializing
    );
}
