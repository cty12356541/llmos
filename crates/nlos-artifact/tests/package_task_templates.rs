//! ADR-0016 决定 1 (W28-B): the additive `tasks` template segment of the
//! package manifest — verification goldens for packages that carry task
//! templates, the segment-tamper and segment-strip negative paths, and the
//! malformed-shape fail-closed variants. The legacy no-segment face and
//! its goldens stay untouched (the G6 hard negative gate lives in both
//! crates' unchanged legacy tests plus an explicit cross-crate install
//! test in `nlos-application`).

mod support;

use nlos_artifact::{
    ArtifactError, ArtifactStore, ContentDigest, PackageEntryRole, PackageTaskKind,
    PackageTaskTemplate, PackageVerificationDecision, SignedPackage, SignedPackageWithTasks,
    VerifyPackageRequest, VerifyPackageWithTasksRequest, package_manifest_message,
    package_manifest_with_tasks_message,
};
use nlos_types::IdempotencyKey;
use support::{
    TestStoreDir, bytes, entry, manifest, publish_artifact, sign_package, sign_templated_package,
    task_template, test_identity,
};

const VERIFY_AT_MS: u64 = 5_000;

fn open_store(name: &str) -> (TestStoreDir, ArtifactStore) {
    let dir = TestStoreDir::new(name);
    let store = ArtifactStore::open(dir.root()).expect("open store");
    (dir, store)
}

fn templated_request(
    signed: &SignedPackageWithTasks,
    key_byte: u8,
) -> VerifyPackageWithTasksRequest<'_> {
    VerifyPackageWithTasksRequest {
        signed,
        idempotency_key: IdempotencyKey::from_bytes([key_byte; 16]),
        verified_at_ms: VERIFY_AT_MS,
    }
}

fn receipt_count(dir: &TestStoreDir) -> i64 {
    let raw = rusqlite::Connection::open(dir.root().join("metadata.db")).expect("raw open");
    raw.query_row(
        "SELECT COUNT(*) FROM package_verification_receipts",
        [],
        |row| row.get(0),
    )
    .expect("count receipts")
}

fn three_template_segment() -> Vec<PackageTaskTemplate> {
    vec![
        task_template([0x01; 16], PackageTaskKind::AgentRole, vec![]),
        task_template([0x02; 16], PackageTaskKind::Executable, vec![[0x01; 16]]),
        task_template(
            [0x03; 16],
            PackageTaskKind::Executable,
            vec![[0x01; 16], [0x02; 16]],
        ),
    ]
}

fn base_manifest(
    artifact: nlos_types::ArtifactId,
    digest: ContentDigest,
) -> nlos_artifact::PackageManifest {
    manifest(
        0x2a,
        4,
        vec![entry(
            "main",
            artifact,
            digest,
            PackageEntryRole::Executable,
        )],
    )
}

#[test]
fn templated_package_verifies_and_binds_artifact_heads() {
    let (_dir, store) = open_store("tasks-happy");
    let identity = test_identity("tasks-happy", 0x61);
    let (artifact, digest) = publish_artifact(&store, 0x21, &bytes(0xe1, 256));

    let signed = sign_templated_package(
        &identity,
        base_manifest(artifact, digest),
        three_template_segment(),
    );
    let decision = store
        .verify_package_with_tasks(&identity.authority, templated_request(&signed, 0x71))
        .expect("verify templated package");
    let PackageVerificationDecision::Verified(receipt) = &decision else {
        panic!("first verification must commit");
    };
    assert_eq!(receipt.package_id, signed.manifest.package_id);
    assert_eq!(receipt.package_version, 4);
    assert_eq!(receipt.entry_count, 1);
    assert_eq!(receipt.signer, identity.binding.principal_id);
    assert_eq!(
        receipt.manifest_digest,
        ContentDigest::from_bytes(package_manifest_with_tasks_message(
            &signed.manifest,
            &signed.tasks
        )),
        "the receipt binds the task-templated message digest"
    );
    // The templated digest is not the legacy digest: the two faces have
    // distinct domain separators, so neither is a valid stand-in for the
    // other.
    assert_ne!(
        receipt.manifest_digest.as_bytes(),
        package_manifest_message(&signed.manifest).as_slice(),
    );

    let inspected = store
        .inspect_package_verification_receipt(receipt.receipt_id)
        .expect("inspect receipt");
    assert_eq!(inspected, *receipt);
}

#[test]
fn templated_verification_replays_byte_identically_across_restart() {
    let (dir, store) = open_store("tasks-replay");
    let identity = test_identity("tasks-replay", 0x62);
    let (artifact, digest) = publish_artifact(&store, 0x22, &bytes(0xe2, 128));

    let signed = sign_templated_package(
        &identity,
        base_manifest(artifact, digest),
        three_template_segment(),
    );
    let request = templated_request(&signed, 0x72);
    let committed = store
        .verify_package_with_tasks(&identity.authority, request)
        .expect("first verify");
    let replayed = store
        .verify_package_with_tasks(&identity.authority, request)
        .expect("replay verify");
    assert!(matches!(
        committed,
        PackageVerificationDecision::Verified(_)
    ));
    assert!(matches!(replayed, PackageVerificationDecision::Replayed(_)));
    assert_eq!(
        committed.receipt(),
        replayed.receipt(),
        "byte-identical replay"
    );
    assert_eq!(receipt_count(&dir), 1, "replay must not add rows");

    drop(store);
    let reopened = ArtifactStore::open(dir.root()).expect("reopen store");
    let after_restart = reopened
        .verify_package_with_tasks(&identity.authority, request)
        .expect("replay after restart");
    assert!(matches!(
        after_restart,
        PackageVerificationDecision::Replayed(_)
    ));
    assert_eq!(committed.receipt(), after_restart.receipt());
    assert_eq!(receipt_count(&dir), 1);

    // The same key with a different segment shape conflicts: flipping one
    // template's dependency changes the signed digest.
    let mut reshaped = sign_templated_package(
        &identity,
        base_manifest(artifact, digest),
        three_template_segment(),
    );
    reshaped.tasks[1].dependency_keys = vec![[0x03; 16]];
    let error = reopened
        .verify_package_with_tasks(&identity.authority, templated_request(&reshaped, 0x72))
        .expect_err("key reuse with a different segment must conflict");
    assert!(matches!(error, ArtifactError::IdempotencyConflict));
}

#[test]
fn verify_with_tasks_fails_closed_on_segment_tamper_and_strip() {
    let (dir, store) = open_store("tasks-tamper");
    let identity = test_identity("tasks-tamper", 0x63);
    let (artifact, digest) = publish_artifact(&store, 0x23, &bytes(0xe3, 512));
    let original = sign_templated_package(
        &identity,
        base_manifest(artifact, digest),
        three_template_segment(),
    );

    // Tamper after signing: flip one template's binding digest.
    let mut binding_flipped = original.clone();
    binding_flipped.tasks[2].binding_digest =
        ContentDigest::of_bytes(b"other-binding").into_bytes();
    let error = store
        .verify_package_with_tasks(
            &identity.authority,
            templated_request(&binding_flipped, 0x73),
        )
        .expect_err("template tampering must fail the signature");
    assert!(matches!(error, ArtifactError::PackageSignatureInvalid));

    // Strip the whole segment and present the base manifest plus the
    // templated signature to the legacy face: the domain-separated
    // templated digest is not a valid signature over the legacy message.
    let stripped = SignedPackage {
        manifest: original.manifest.clone(),
        signer: original.signer,
        signature: original.signature,
    };
    let error = store
        .verify_package(
            &identity.authority,
            VerifyPackageRequest {
                signed: &stripped,
                idempotency_key: IdempotencyKey::from_bytes([0x73; 16]),
                verified_at_ms: VERIFY_AT_MS,
            },
        )
        .expect_err("a stripped segment must not downgrade the signature");
    assert!(
        matches!(error, ArtifactError::PackageSignatureInvalid),
        "expected PackageSignatureInvalid, got {error}"
    );

    // Inject a segment onto a legacy signature: equally impossible.
    let legacy = sign_package(&identity, base_manifest(artifact, digest));
    let injected = SignedPackageWithTasks {
        manifest: legacy.manifest.clone(),
        tasks: three_template_segment(),
        signer: legacy.signer,
        signature: legacy.signature,
    };
    let error = store
        .verify_package_with_tasks(&identity.authority, templated_request(&injected, 0x73))
        .expect_err("an injected segment must fail the templated signature");
    assert!(matches!(error, ArtifactError::PackageSignatureInvalid));
    assert_eq!(
        receipt_count(&dir),
        0,
        "every failed verification must leave zero durable state"
    );
}

#[test]
fn templated_segment_shape_is_validated_fail_closed() {
    let (_dir, store) = open_store("tasks-shape");
    let identity = test_identity("tasks-shape", 0x64);
    let (artifact, digest) = publish_artifact(&store, 0x24, &bytes(0xe4, 64));
    let base = base_manifest(artifact, digest);

    // Shape validation runs before signature verification, mirroring the
    // legacy face's shape test: the dummy signatures below never reach
    // Ed25519, they only exercise the typed shape failures.
    let shape_package = |tasks: Vec<PackageTaskTemplate>| SignedPackageWithTasks {
        manifest: base.clone(),
        tasks,
        signer: identity.binding.principal_id,
        signature: [0_u8; 64],
    };
    let shape_error = |package: &SignedPackageWithTasks| {
        store
            .verify_package_with_tasks(&identity.authority, templated_request(package, 0x74))
            .expect_err("malformed segment must fail closed")
    };

    let empty = shape_error(&shape_package(vec![]));
    assert!(matches!(empty, ArtifactError::PackageManifestInvalid(_)));

    let duplicated = shape_error(&shape_package(vec![
        task_template([0x05; 16], PackageTaskKind::AgentRole, vec![]),
        task_template([0x05; 16], PackageTaskKind::Executable, vec![]),
    ]));
    assert!(matches!(
        duplicated,
        ArtifactError::PackageManifestInvalid(_)
    ));

    let self_dependency = shape_error(&shape_package(vec![task_template(
        [0x06; 16],
        PackageTaskKind::AgentRole,
        vec![[0x06; 16]],
    )]));
    assert!(matches!(
        self_dependency,
        ArtifactError::PackageManifestInvalid(_)
    ));

    let dangling = shape_error(&shape_package(vec![task_template(
        [0x07; 16],
        PackageTaskKind::AgentRole,
        vec![[0x9e; 16]],
    )]));
    assert!(matches!(dangling, ArtifactError::PackageManifestInvalid(_)));

    let mut too_many_dependencies = Vec::new();
    for index in 0..=nlos_artifact::MAX_TASK_DEPENDENCIES_PER_TEMPLATE {
        let mut key = [0x08; 16];
        key[15] = u8::try_from(index & 0xff).expect("fits in u8");
        too_many_dependencies.push(key);
    }
    let over_dep_bound = shape_error(&shape_package(vec![task_template(
        [0x09; 16],
        PackageTaskKind::AgentRole,
        too_many_dependencies,
    )]));
    assert!(matches!(
        over_dep_bound,
        ArtifactError::PackageManifestInvalid(_)
    ));

    let templates: Vec<PackageTaskTemplate> = (0..=nlos_artifact::MAX_TASK_TEMPLATES_PER_MANIFEST)
        .map(|index| {
            let mut key = [0x0a; 16];
            key[15] = u8::try_from(index & 0xff).expect("fits in u8");
            key[14] = u8::try_from((index >> 8) & 0xff).expect("fits in u8");
            task_template(key, PackageTaskKind::AgentRole, vec![])
        })
        .collect();
    let over_template_bound = shape_error(&shape_package(templates));
    assert!(matches!(
        over_template_bound,
        ArtifactError::PackageManifestInvalid(_)
    ));
}
