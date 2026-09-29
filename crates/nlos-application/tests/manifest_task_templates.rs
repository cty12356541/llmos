//! ADR-0016 决定 1 / W28-B template half:
//!
//! - **G6 hard negative gate**: an old signed package (no `tasks`
//!   segment) still verifies and installs byte-identically with the
//!   template face present in the same build — the legacy goldens in
//!   `application_authority.rs` / `package_signature.rs` stay untouched,
//!   and this file re-proves the install path end to end.
//! - A task-templated package verifies through the additive face and
//!   installs through the same receipt-consuming path as any legacy
//!   package.
//! - **PLAN-OVERRIDE-001 compile equivalence**: a manifest `tasks`
//!   segment compiles to *exactly* the plan proposal a caller would
//!   declare directly (byte-equal and digest-equal).

mod support;

use ed25519_dalek::Signer;
use sha2::{Digest, Sha256};

use nlos_application::{
    InstallApplicationRequest, InstallDecision, TaskTemplateError, compile_task_templates,
};
use nlos_artifact::{
    ContentDigest, PackageEntryRole, PackageManifest, PackageManifestEntry, PackageTaskKind,
    SignedPackage, package_manifest_message,
};
use nlos_plan::{ApplyPlanRevisionRequest, PlanNodeDeclaration, PlanNodeKind};
use nlos_types::{IdempotencyKey, PackageId};
use support::{TestStack, open_authority, task_template};

const VERIFY_AT_MS: u64 = 6_000;
const INSTALL_AT_MS: u64 = 7_000;

fn legacy_signed_package(
    stack: &TestStack,
    package_seed: u8,
    version: u64,
    payload: &[u8],
) -> (SignedPackage, nlos_types::ArtifactId, ContentDigest) {
    let (artifact_id, digest) = stack.publish_artifact(0x40, payload);
    let manifest = PackageManifest {
        package_id: PackageId::from_bytes([package_seed; 16]),
        version,
        entries: vec![PackageManifestEntry {
            name: "main".to_string(),
            artifact_id,
            digest,
            role: PackageEntryRole::Executable,
        }],
    };
    let message = package_manifest_message(&manifest);
    let signed = SignedPackage {
        manifest,
        signer: stack.identity.binding.principal_id,
        signature: stack.identity.key.sign(&message).to_bytes(),
    };
    (signed, artifact_id, digest)
}

#[test]
fn g6_old_shape_package_verifies_and_installs_byte_identically() {
    let stack = TestStack::new("w28b-g6-old", 0x81);
    let authority = open_authority(stack.root.root());
    let (signed, artifact_id, digest) = legacy_signed_package(&stack, 0x50, 5, b"legacy-payload");

    // Verify through the untouched legacy face; the receipt's digest is
    // the exact legacy manifest message.
    let decision = stack
        .artifacts
        .verify_package(
            &stack.identity.authority,
            nlos_artifact::VerifyPackageRequest {
                signed: &signed,
                idempotency_key: IdempotencyKey::from_bytes([0x91; 16]),
                verified_at_ms: VERIFY_AT_MS,
            },
        )
        .expect("legacy package must verify with the template face present");
    let receipt = decision.receipt().clone();
    assert_eq!(
        receipt.manifest_digest,
        ContentDigest::from_bytes(package_manifest_message(&signed.manifest)),
        "the additive segment must not move one byte of the legacy framing"
    );
    assert_eq!(receipt.package_version, 5);
    assert_eq!(receipt.entry_count, 1);
    assert_eq!(receipt.signer, stack.identity.binding.principal_id);

    // Install through the unchanged receipt-consuming path, then replay:
    // byte-identical durable facts, no generation double-jump.
    let installed = match authority
        .install_application(
            &stack.artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: receipt.receipt_id,
                idempotency_key: IdempotencyKey::from_bytes([0x92; 16]),
                installed_at_ms: INSTALL_AT_MS,
            },
        )
        .expect("legacy package must install")
    {
        InstallDecision::Installed(receipt) => receipt,
        InstallDecision::Replayed(receipt) => {
            panic!("fresh key cannot replay, got {receipt:?}")
        }
    };
    assert_eq!(installed.package_id, PackageId::from_bytes([0x50; 16]));
    assert_eq!(installed.package_manifest_digest, receipt.manifest_digest);
    assert_eq!(installed.package_version, 5);
    assert_eq!(installed.entry_count, 1);
    assert_eq!(installed.installer_principal, receipt.signer);

    let replayed = match authority
        .install_application(
            &stack.artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: receipt.receipt_id,
                idempotency_key: IdempotencyKey::from_bytes([0x92; 16]),
                installed_at_ms: INSTALL_AT_MS,
            },
        )
        .expect("install replay")
    {
        InstallDecision::Replayed(receipt) => receipt,
        InstallDecision::Installed(receipt) => {
            panic!("expected replay, got fresh install {receipt:?}")
        }
    };
    assert_eq!(installed, replayed, "byte-identical durable replay");
    assert_eq!(
        installed.installation_generation, replayed.installation_generation,
        "no generation double-jump"
    );
    assert_eq!(artifact_id.as_bytes()[0], 0x40);
    assert_ne!(digest, ContentDigest::of_bytes(b"other"));
}

#[test]
fn templated_package_installs_through_the_same_receipt_path() {
    let stack = TestStack::new("w28b-tasks-install", 0x82);
    let authority = open_authority(stack.root.root());
    let tasks = vec![
        task_template([0x01; 16], PackageTaskKind::AgentRole, vec![]),
        task_template([0x02; 16], PackageTaskKind::Executable, vec![[0x01; 16]]),
    ];
    let (verification, _signed) = stack.verify_templated_package(
        0x51,
        2,
        tasks,
        IdempotencyKey::from_bytes([0x93; 16]),
        VERIFY_AT_MS,
    );

    let installed = match authority
        .install_application(
            &stack.artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: verification.receipt_id,
                idempotency_key: IdempotencyKey::from_bytes([0x94; 16]),
                installed_at_ms: INSTALL_AT_MS,
            },
        )
        .expect("templated package must install")
    {
        InstallDecision::Installed(receipt) => receipt,
        InstallDecision::Replayed(receipt) => {
            panic!("fresh key cannot replay, got {receipt:?}")
        }
    };
    assert_eq!(installed.package_id, PackageId::from_bytes([0x51; 16]));
    assert_eq!(
        installed.package_manifest_digest, verification.manifest_digest,
        "the install binds the task-templated digest"
    );
    assert_eq!(installed.package_version, 2);
    assert_eq!(installed.installer_principal, verification.signer);
}

/// Canonical test-side serialization of one declared node set: every
/// declaration field in fixed order, so proposal equality can be checked
/// digest-wise as well as struct-wise.
fn declaration_digest(nodes: &[PlanNodeDeclaration]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"w28b-plan-override-001-test-digest/v1");
    hasher.update(u64::try_from(nodes.len()).unwrap_or(u64::MAX).to_be_bytes());
    for node in nodes {
        hasher.update(node.node_key);
        hasher.update([match node.kind {
            PlanNodeKind::AgentRole => 1_u8,
            PlanNodeKind::Executable => 2_u8,
        }]);
        hasher.update(node.binding_digest);
        hasher.update(
            u64::try_from(node.dependency_keys.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        for dependency in &node.dependency_keys {
            hasher.update(*dependency);
        }
        hasher.update(node.input_selectors_digest);
        hasher.update(node.output_contract_digest);
        hasher.update(node.policy_digest);
        hasher.update(node.resource_ceiling_digest);
    }
    hasher.finalize().into()
}

#[test]
fn plan_override_001_templated_segment_compiles_to_the_direct_plan_proposal() {
    let stack = TestStack::new("w28b-plan-override", 0x83);
    let tasks = vec![
        task_template([0x01; 16], PackageTaskKind::AgentRole, vec![]),
        task_template(
            [0x02; 16],
            PackageTaskKind::Executable,
            vec![[0x03; 16], [0x01; 16]],
        ),
        task_template([0x03; 16], PackageTaskKind::Executable, vec![[0x01; 16]]),
    ];
    let (_receipt, signed) = stack.verify_templated_package(
        0x52,
        1,
        tasks,
        IdempotencyKey::from_bytes([0x95; 16]),
        VERIFY_AT_MS,
    );

    let idempotency_key = IdempotencyKey::from_bytes([0x96; 16]);
    let compiled = compile_task_templates(&signed, idempotency_key, 8_000)
        .expect("a verified segment must compile");

    // The proposal a caller would declare directly with the same bytes.
    let direct = ApplyPlanRevisionRequest {
        plan_id: None,
        nodes: vec![
            PlanNodeDeclaration {
                node_key: [0x01; 16],
                kind: PlanNodeKind::AgentRole,
                binding_digest: ContentDigest::of_bytes(&[0x01; 16]).into_bytes(),
                dependency_keys: vec![],
                input_selectors_digest: ContentDigest::of_bytes(b"input-selectors").into_bytes(),
                output_contract_digest: ContentDigest::of_bytes(b"output-contract").into_bytes(),
                policy_digest: ContentDigest::of_bytes(b"policy").into_bytes(),
                resource_ceiling_digest: ContentDigest::of_bytes(b"resource-ceiling").into_bytes(),
                conditions: None,
            },
            PlanNodeDeclaration {
                node_key: [0x02; 16],
                kind: PlanNodeKind::Executable,
                binding_digest: ContentDigest::of_bytes(&[0x02; 16]).into_bytes(),
                dependency_keys: vec![[0x03; 16], [0x01; 16]],
                input_selectors_digest: ContentDigest::of_bytes(b"input-selectors").into_bytes(),
                output_contract_digest: ContentDigest::of_bytes(b"output-contract").into_bytes(),
                policy_digest: ContentDigest::of_bytes(b"policy").into_bytes(),
                resource_ceiling_digest: ContentDigest::of_bytes(b"resource-ceiling").into_bytes(),
                conditions: None,
            },
            PlanNodeDeclaration {
                node_key: [0x03; 16],
                kind: PlanNodeKind::Executable,
                binding_digest: ContentDigest::of_bytes(&[0x03; 16]).into_bytes(),
                dependency_keys: vec![[0x01; 16]],
                input_selectors_digest: ContentDigest::of_bytes(b"input-selectors").into_bytes(),
                output_contract_digest: ContentDigest::of_bytes(b"output-contract").into_bytes(),
                policy_digest: ContentDigest::of_bytes(b"policy").into_bytes(),
                resource_ceiling_digest: ContentDigest::of_bytes(b"resource-ceiling").into_bytes(),
                conditions: None,
            },
        ],
        idempotency_key,
        applied_at_ms: 8_000,
    };

    assert_eq!(
        compiled, direct,
        "byte-equal: the segment compiles to exactly the direct declaration"
    );
    assert_eq!(
        declaration_digest(&compiled.nodes),
        declaration_digest(&direct.nodes),
        "digest-equal over canonical declaration bytes"
    );

    // Dependency order is declaration data: the compiled proposal keeps
    // the declared order bitwise (a reordered direct declaration is a
    // different proposal, exactly as at the plan face).
    let mut reordered = direct.clone();
    reordered.nodes[1].dependency_keys = vec![[0x01; 16], [0x03; 16]];
    assert_ne!(compiled, reordered);

    // The compile is deterministic: same inputs, same proposal.
    let again = compile_task_templates(&signed, idempotency_key, 8_000)
        .expect("second compile must succeed");
    assert_eq!(compiled, again);
}

#[test]
fn compile_refuses_malformed_segments_fail_closed() {
    let stack = TestStack::new("w28b-compile-negative", 0x84);
    let duplicate_keys = vec![
        task_template([0x07; 16], PackageTaskKind::AgentRole, vec![]),
        task_template([0x07; 16], PackageTaskKind::Executable, vec![]),
    ];
    let malformed = nlos_artifact::SignedPackageWithTasks {
        manifest: PackageManifest {
            package_id: PackageId::from_bytes([0x53; 16]),
            version: 1,
            entries: vec![PackageManifestEntry {
                name: "main".to_string(),
                artifact_id: nlos_types::ArtifactId::from_bytes([0x41; 16]),
                digest: ContentDigest::of_bytes(b"payload"),
                role: PackageEntryRole::Executable,
            }],
        },
        tasks: duplicate_keys,
        signer: stack.identity.binding.principal_id,
        signature: [0_u8; 64],
    };
    let error = compile_task_templates(&malformed, IdempotencyKey::from_bytes([0x97; 16]), 9_000)
        .expect_err("a malformed segment must fail closed");
    assert!(matches!(error, TaskTemplateError::InvalidSegment(_)));
}
