//! B-ARTIFACT-003: minimal signed Package envelope verification — normal
//! sign/verify, tampered manifest, tampered entry content, unknown signer,
//! revoked key, idempotent replay, and restart replay.

mod support;

use ed25519_dalek::{Signer, SigningKey};
use nlos_artifact::{
    ArtifactError, ArtifactStore, ContentDigest, PackageEntryRole, PackageVerificationDecision,
    SignedPackage, VerifyPackageRequest, package_manifest_message,
};
use nlos_identity::RevokeKeyRequest;
use nlos_types::{Generation, IdempotencyKey, PackageId, PrincipalId};
use support::{
    TestStoreDir, bytes, entry, manifest, publish_artifact, put, sign_package, test_identity,
};

const VERIFY_AT_MS: u64 = 5_000;

fn open_store(name: &str) -> (TestStoreDir, ArtifactStore) {
    let dir = TestStoreDir::new(name);
    let store = ArtifactStore::open(dir.root()).expect("open store");
    (dir, store)
}

fn single_entry_manifest(
    seed: u8,
    version: u64,
    artifact: nlos_types::ArtifactId,
    digest: ContentDigest,
) -> nlos_artifact::PackageManifest {
    manifest(
        seed,
        version,
        vec![entry("only", artifact, digest, PackageEntryRole::Data)],
    )
}

fn verify_request(signed: &SignedPackage, key_byte: u8) -> VerifyPackageRequest<'_> {
    VerifyPackageRequest {
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

#[test]
fn signed_package_verifies_and_binds_artifact_heads() {
    let (_dir, store) = open_store("package-happy");
    let identity = test_identity("package-happy", 0x51);
    let (kernel, kernel_digest) = publish_artifact(&store, 0x01, &bytes(0xa1, 512));
    let (service, service_digest) = publish_artifact(&store, 0x02, &bytes(0xa2, 1024));

    let signed = sign_package(
        &identity,
        manifest(
            0x10,
            7,
            vec![
                entry(
                    "kernel",
                    kernel,
                    kernel_digest,
                    PackageEntryRole::Executable,
                ),
                entry(
                    "service",
                    service,
                    service_digest,
                    PackageEntryRole::BackgroundService,
                ),
            ],
        ),
    );
    let decision = store
        .verify_package(&identity.authority, verify_request(&signed, 0x61))
        .expect("verify signed package");
    let PackageVerificationDecision::Verified(receipt) = &decision else {
        panic!("first verification must commit");
    };
    assert_eq!(receipt.package_id, PackageId::from_bytes([0x10; 16]));
    assert_eq!(receipt.package_version, 7);
    assert_eq!(receipt.entry_count, 2);
    assert_eq!(receipt.signer, identity.binding.principal_id);
    assert_eq!(receipt.key_id, identity.binding.key_id);
    assert_eq!(receipt.key_generation, Generation::INITIAL);
    assert_eq!(
        receipt.manifest_digest,
        ContentDigest::from_bytes(package_manifest_message(&signed.manifest))
    );
    assert_eq!(receipt.verified_at_ms, VERIFY_AT_MS);

    // The receipt is durable and inspectable by id.
    let inspected = store
        .inspect_package_verification_receipt(receipt.receipt_id)
        .expect("inspect receipt");
    assert_eq!(inspected, *receipt);
}

#[test]
fn verify_package_is_idempotent_and_replays_across_restart() {
    let (dir, store) = open_store("package-replay");
    let identity = test_identity("package-replay", 0x52);
    let (artifact, digest) = publish_artifact(&store, 0x03, &bytes(0xa3, 256));
    let signed = sign_package(&identity, single_entry_manifest(0x11, 1, artifact, digest));
    let request = verify_request(&signed, 0x62);

    let committed = store
        .verify_package(&identity.authority, request)
        .expect("first verify");
    let replayed = store
        .verify_package(&identity.authority, request)
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

    // Restart replay: the durable receipt is returned unchanged.
    drop(store);
    let reopened = ArtifactStore::open(dir.root()).expect("reopen store");
    let after_restart = reopened
        .verify_package(&identity.authority, request)
        .expect("replay after restart");
    assert!(matches!(
        after_restart,
        PackageVerificationDecision::Replayed(_)
    ));
    assert_eq!(committed.receipt(), after_restart.receipt());
    assert_eq!(receipt_count(&dir), 1);

    // A different request shape under the same idempotency key conflicts.
    let other = sign_package(&identity, single_entry_manifest(0x11, 2, artifact, digest));
    let error = reopened
        .verify_package(&identity.authority, verify_request(&other, 0x62))
        .expect_err("key reuse with different signed package must conflict");
    assert!(matches!(error, ArtifactError::IdempotencyConflict));
}

#[test]
fn verify_package_replay_survives_key_revocation() {
    let (dir, store) = open_store("package-replay-revoked");
    let identity = test_identity("package-replay-revoked", 0x53);
    let (artifact, digest) = publish_artifact(&store, 0x04, &bytes(0xa4, 128));
    let signed = sign_package(&identity, single_entry_manifest(0x12, 1, artifact, digest));
    let request = verify_request(&signed, 0x63);
    let committed = store
        .verify_package(&identity.authority, request)
        .expect("verify before revocation");

    identity
        .authority
        .revoke_key(RevokeKeyRequest {
            key_id: identity.binding.key_id,
            expected_key_generation: Generation::INITIAL,
            expected_identity_snapshot_id: identity.binding.identity_snapshot_id,
            idempotency_key: IdempotencyKey::from_bytes([0x6f; 16]),
            revoked_at_ms: 6_000,
        })
        .expect("revoke key");

    // The durable receipt is the authority: replay never re-verifies.
    drop(store);
    let reopened = ArtifactStore::open(dir.root()).expect("reopen");
    let replayed = reopened
        .verify_package(&identity.authority, request)
        .expect("replay after revocation");
    assert!(matches!(replayed, PackageVerificationDecision::Replayed(_)));
    assert_eq!(committed.receipt(), replayed.receipt());

    // Fresh verifications fail closed on the revoked key.
    let next = sign_package(&identity, single_entry_manifest(0x13, 1, artifact, digest));
    let error = reopened
        .verify_package(&identity.authority, verify_request(&next, 0x64))
        .expect_err("fresh verification must fail on revoked key");
    assert!(matches!(error, ArtifactError::PackageKeyRevoked));
}

#[test]
fn verify_package_fails_closed_on_tampered_manifest() {
    let (dir, store) = open_store("package-tamper-manifest");
    let identity = test_identity("package-tamper-manifest", 0x54);
    let (artifact, digest) = publish_artifact(&store, 0x05, &bytes(0xa5, 512));

    let mut tampered = sign_package(&identity, single_entry_manifest(0x14, 1, artifact, digest));
    // Tamper after signing: flip the declared version.
    tampered.manifest.version = 2;
    let error = store
        .verify_package(&identity.authority, verify_request(&tampered, 0x65))
        .expect_err("tampered manifest must fail");
    assert!(
        matches!(error, ArtifactError::PackageSignatureInvalid),
        "expected PackageSignatureInvalid, got {error}"
    );
    assert_eq!(
        receipt_count(&dir),
        0,
        "failed verification must leave zero durable state"
    );

    // Tampering a single entry digest is equally a signature failure.
    let mut entry_flipped =
        sign_package(&identity, single_entry_manifest(0x14, 1, artifact, digest));
    entry_flipped.manifest.entries[0].digest = ContentDigest::of_bytes(b"other");
    let error = store
        .verify_package(&identity.authority, verify_request(&entry_flipped, 0x65))
        .expect_err("entry tampering must fail the signature");
    assert!(matches!(error, ArtifactError::PackageSignatureInvalid));
    assert_eq!(receipt_count(&dir), 0);
}

#[test]
fn verify_package_fails_closed_on_tampered_entry_content() {
    let (_dir, store) = open_store("package-tamper-content");
    let identity = test_identity("package-tamper-content", 0x55);
    let (artifact, stale_digest) = publish_artifact(&store, 0x06, &bytes(0xa6, 512));

    // Manifest correctly signed over a digest the artifact no longer has:
    // the head advanced after signing.
    let signed = sign_package(
        &identity,
        single_entry_manifest(0x15, 1, artifact, stale_digest),
    );
    store
        .put_revision(put(artifact, 1, &bytes(0xb6, 512)))
        .expect("advance head");
    let error = store
        .verify_package(&identity.authority, verify_request(&signed, 0x66))
        .expect_err("stale binding must fail");
    match error {
        ArtifactError::PackageTampered {
            entry,
            expected,
            actual: Some(actual),
        } => {
            assert_eq!(entry, "only");
            assert_eq!(expected, stale_digest);
            assert_eq!(actual, ContentDigest::of_bytes(&bytes(0xb6, 512)));
        }
        other => panic!("expected PackageTampered, got {other}"),
    }

    // An existing artifact with no revisions yet has no head to bind.
    let empty_spec = support::artifact_spec(0x07);
    store.create_artifact(empty_spec.clone()).expect("create");
    let signed_empty = sign_package(
        &identity,
        manifest(
            0x16,
            1,
            vec![entry(
                "empty",
                empty_spec.artifact_id,
                ContentDigest::of_bytes(b"never"),
                PackageEntryRole::Data,
            )],
        ),
    );
    let error = store
        .verify_package(&identity.authority, verify_request(&signed_empty, 0x67))
        .expect_err("headless artifact must fail binding");
    assert!(
        matches!(error, ArtifactError::PackageTampered { actual: None, .. }),
        "expected PackageTampered with absent head, got {error}"
    );

    // An entry naming an artifact the store never saw fails typed.
    let ghost = support::artifact_id(0x7f);
    let signed_ghost = sign_package(
        &identity,
        manifest(
            0x17,
            1,
            vec![entry(
                "ghost",
                ghost,
                ContentDigest::of_bytes(b"ghost"),
                PackageEntryRole::Data,
            )],
        ),
    );
    let error = store
        .verify_package(&identity.authority, verify_request(&signed_ghost, 0x68))
        .expect_err("unknown artifact must fail binding");
    assert!(matches!(
        error,
        ArtifactError::ArtifactNotFound(ref not_found) if *not_found == ghost
    ));
}

#[test]
fn verify_package_fails_closed_on_unknown_principal() {
    let (_dir, store) = open_store("package-unknown-signer");
    let identity = test_identity("package-unknown-signer", 0x56);
    let (artifact, digest) = publish_artifact(&store, 0x08, &bytes(0xa8, 256));

    // A principal the identity authority never bootstrapped, carrying a
    // well-formed signature over the correct manifest digest.
    let stranger = SigningKey::from_bytes(&[0xde; 32]);
    let package = single_entry_manifest(0x18, 1, artifact, digest);
    let signed = SignedPackage {
        manifest: package.clone(),
        signer: PrincipalId::from_bytes([0x7e; 16]),
        signature: stranger
            .sign(&package_manifest_message(&package))
            .to_bytes(),
    };
    let error = store
        .verify_package(&identity.authority, verify_request(&signed, 0x69))
        .expect_err("unknown signer must fail");
    assert!(matches!(
        error,
        ArtifactError::PackagePrincipalUnknown(id) if id == PrincipalId::from_bytes([0x7e; 16])
    ));
}

#[test]
fn verify_package_validates_manifest_shape() {
    let (_dir, store) = open_store("package-shape");
    let identity = test_identity("package-shape", 0x57);

    // Shape validation runs before signature verification, so the unsigned
    // manifests below still exercise the typed shape failures.
    let empty = sign_package(&identity, manifest(0x19, 1, vec![]));
    let error = store
        .verify_package(&identity.authority, verify_request(&empty, 0x6a))
        .expect_err("empty manifest must fail");
    assert!(matches!(error, ArtifactError::PackageManifestInvalid(_)));

    let duplicated = sign_package(
        &identity,
        manifest(
            0x19,
            1,
            vec![
                entry(
                    "same",
                    support::artifact_id(0x01),
                    ContentDigest::of_bytes(b"x"),
                    PackageEntryRole::Data,
                ),
                entry(
                    "same",
                    support::artifact_id(0x02),
                    ContentDigest::of_bytes(b"y"),
                    PackageEntryRole::Data,
                ),
            ],
        ),
    );
    let error = store
        .verify_package(&identity.authority, verify_request(&duplicated, 0x6a))
        .expect_err("duplicate entry names must fail");
    assert!(matches!(error, ArtifactError::PackageManifestInvalid(_)));

    let unnamed = sign_package(
        &identity,
        manifest(
            0x19,
            1,
            vec![entry(
                "",
                support::artifact_id(0x01),
                ContentDigest::of_bytes(b"x"),
                PackageEntryRole::Data,
            )],
        ),
    );
    let error = store
        .verify_package(&identity.authority, verify_request(&unnamed, 0x6a))
        .expect_err("empty entry name must fail");
    assert!(matches!(error, ArtifactError::PackageManifestInvalid(_)));
}
