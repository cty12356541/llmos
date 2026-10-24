//! Stage D remainder: `ControlCommand::InspectApplication` through the
//! existing `ApplicationAuthorityInspector`. The desktop shell already
//! projects `ApplicationInspected`; this lane only passes the inspector
//! the resource-cost path already passes for `ResourceAuthority`.

#![cfg(all(unix, feature = "dev-fixture"))]

use std::path::PathBuf;

use ed25519_dalek::Signer;
use llmos_desktop_lib::devfixture::DevFixture;
use llmos_desktop_lib::dto::OutcomeDto;
use llmos_desktop_lib::ipc::dispatch_application_inspect;
use nlos_application::{ApplicationAuthority, InstallApplicationRequest, InstallDecision};
use nlos_artifact::{
    ArtifactStore, ContentDigest, CreateArtifactSpec, PackageEntryRole, PackageManifest,
    PackageManifestEntry, ProvenanceSourceTriple, PutRevisionRequest, SignedPackage,
    VerifyPackageRequest, package_manifest_message,
};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_system_control::control::{ControlCommand, ControlReceipt, receipt_to_hex};
use nlos_types::{ApplicationId, ArtifactId, IdempotencyKey, PackageId, PrincipalId};

fn write_key_file(fixture: &DevFixture, label: &str) -> String {
    let path = std::env::temp_dir().join(format!(
        "llmosdt-test-key-{label}-{}.key",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    fixture.write_key_file(&path).expect("write key file");
    path.display().to_string()
}

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "llmosdt-app-inspect-{name}-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp root");
    root
}

fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

struct PackageStack {
    artifacts: ArtifactStore,
    identity: IdentityAuthority,
    key: ed25519_dalek::SigningKey,
    principal: PrincipalId,
    _root: PathBuf,
}

impl PackageStack {
    fn new(name: &str, seed: u8) -> Self {
        let root = temp_root(name);
        let artifacts = ArtifactStore::open(root.join("art")).expect("open artifact store");
        let identity = IdentityAuthority::open(root.join("identity")).expect("open identity");
        let key = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        let binding = identity
            .bootstrap_principal(BootstrapPrincipalRequest {
                principal_profile_digest: [seed.wrapping_add(1); 32],
                control_domain_policy_digest: [seed.wrapping_add(2); 32],
                public_key: key.verifying_key().to_bytes(),
                key_purpose: KeyPurpose::SemanticSigning,
                key_valid_from_ms: 0,
                key_valid_until_ms: 10_000,
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
                created_at_ms: 0,
            })
            .expect("bootstrap principal")
            .binding();
        Self {
            artifacts,
            identity,
            key,
            principal: binding.principal_id,
            _root: root,
        }
    }

    fn verify_package(
        &self,
        package_seed: u8,
        key: IdempotencyKey,
        verified_at_ms: u64,
    ) -> nlos_artifact::PackageVerificationReceipt {
        let payload = b"application inspect payload";
        let artifact_id = ArtifactId::from_bytes([package_seed; 16]);
        self.artifacts
            .create_artifact(CreateArtifactSpec {
                artifact_id,
                idempotency_key: IdempotencyKey::from_bytes(
                    [(0xa0_u8).wrapping_add(package_seed); 16],
                ),
                content_type: "application/octet-stream".to_string(),
                application_id: Some(ApplicationId::from_bytes(
                    [(0xb0_u8).wrapping_add(package_seed); 16],
                )),
                owner: Some(format!("sample-{package_seed}")),
                created_at_ms: 1_000,
            })
            .expect("create artifact");
        self.artifacts
            .put_revision(PutRevisionRequest {
                artifact_id,
                expected_head_revision: 0,
                bytes: payload,
                created_at_ms: 5_000,
                provenance: ProvenanceSourceTriple {
                    source_a: [0xc0_u8.wrapping_add(package_seed); 16],
                    source_b: [0xd0_u8.wrapping_add(package_seed); 16],
                    source_digest: ContentDigest::of_bytes(payload),
                },
            })
            .expect("put revision");
        let manifest = PackageManifest {
            package_id: PackageId::from_bytes([package_seed; 16]),
            version: 1,
            entries: vec![PackageManifestEntry {
                name: "main".to_string(),
                artifact_id,
                digest: ContentDigest::of_bytes(payload),
                role: PackageEntryRole::Executable,
            }],
        };
        let message = package_manifest_message(&manifest);
        let signed = SignedPackage {
            manifest,
            signer: self.principal,
            signature: self.key.sign(&message).to_bytes(),
        };
        self.artifacts
            .verify_package(
                &self.identity,
                VerifyPackageRequest {
                    signed: &signed,
                    idempotency_key: key,
                    verified_at_ms,
                },
            )
            .expect("verify package")
            .receipt()
            .clone()
    }
}

#[tokio::test]
async fn application_inspect_matches_authority_head() {
    let mut fixture = DevFixture::spawn("a1").expect("fixture");
    let key_file = write_key_file(&fixture, "a1");
    let _loops = fixture.serve_forever();

    let stack = PackageStack::new("a1-pkg", 0xA1);
    let authority_root = temp_root("a1-authority");
    let authority = ApplicationAuthority::open(&authority_root).expect("open authority");
    let verification = stack.verify_package(0xA2, IdempotencyKey::from_bytes([0x11; 16]), 6_000);
    let installed = match authority
        .install_application(
            &stack.artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: verification.receipt_id,
                idempotency_key: IdempotencyKey::from_bytes([0x12; 16]),
                installed_at_ms: 7_000,
            },
        )
        .expect("install")
    {
        InstallDecision::Installed(receipt) => receipt,
        InstallDecision::Replayed(receipt) => panic!("fresh key cannot replay, got {receipt:?}"),
    };
    let head = authority
        .inspect_application(verification.package_id)
        .expect("direct inspect")
        .expect("installed application");

    let receipt = dispatch_application_inspect(
        fixture
            .socket_authenticated()
            .display()
            .to_string()
            .as_str(),
        fixture.principal_hex(),
        &key_file,
        Some(&authority),
        *verification.package_id.as_bytes(),
    )
    .await
    .expect("application inspect dispatch");
    let OutcomeDto::ApplicationInspected {
        package_id_hex,
        application_id_hex,
        package_manifest_digest_hex,
        current_installation_generation,
        status,
        created_at_ms,
        updated_at_ms,
    } = &receipt.outcome
    else {
        panic!("expected application inspection, got {:?}", receipt.outcome);
    };
    assert_eq!(*package_id_hex, lower_hex(head.package_id.as_bytes()));
    assert_eq!(
        *application_id_hex,
        lower_hex(head.application_id.as_bytes())
    );
    assert_eq!(
        *package_manifest_digest_hex,
        lower_hex(head.package_manifest_digest.as_bytes())
    );
    assert_eq!(
        *current_installation_generation,
        head.current_installation_generation.get()
    );
    assert_eq!(*status, 1, "Installed encodes as 1");
    assert_eq!(*created_at_ms, head.created_at_ms);
    assert_eq!(*updated_at_ms, head.updated_at_ms);
    assert_eq!(
        installed.package_manifest_digest,
        head.package_manifest_digest
    );

    let plain: ControlReceipt = nlos_system_control::control::dispatch_over_socket(
        fixture.socket_plain(),
        &ControlCommand::InspectApplication {
            package_id: *verification.package_id.as_bytes(),
        },
        None,
        None,
        Some(
            &nlos_system_control::application_inspector::ApplicationAuthorityInspector::new(
                &authority,
            ),
        ),
    )
    .await
    .expect("plain dispatch");
    assert_eq!(receipt.receipt_hex, receipt_to_hex(&plain));

    let _ = std::fs::remove_file(&key_file);
}

#[tokio::test]
async fn unwired_application_inspect_is_typed_not_found() {
    let mut fixture = DevFixture::spawn("a2").expect("fixture");
    let key_file = write_key_file(&fixture, "a2");
    let _loops = fixture.serve_forever();

    let receipt = dispatch_application_inspect(
        fixture
            .socket_authenticated()
            .display()
            .to_string()
            .as_str(),
        fixture.principal_hex(),
        &key_file,
        None,
        [0x77; 16],
    )
    .await
    .expect("unwired dispatch itself succeeds");
    let OutcomeDto::Failure {
        code, safe_message, ..
    } = &receipt.outcome
    else {
        panic!("expected failure outcome, got {:?}", receipt.outcome);
    };
    assert!(code.contains("NOT_FOUND"), "code was {code}");
    assert!(safe_message.contains("not wired"));

    let _ = std::fs::remove_file(&key_file);
}
