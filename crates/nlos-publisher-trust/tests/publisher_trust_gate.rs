//! End-to-end integration and adversarial verification gate for
//! `nlos-publisher-trust` (`W79-E4-PUBLISHER-TRUST`, `ADR-0027` section 2.3;
//! `[PKG-IMM-001]`, `[PKG-APPID-001]`, `[PKG-SEP-001]`, `[DUI-OFFLINE-001]`).

#![allow(clippy::too_many_lines)] // End-to-end multi-step PKI & SQLite restart scenarios keep linear narrative in one test.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_capability::custody::{
    CustodyTier, KeyPurpose, ProvisionCustodyKeyRequest, SigningKeyCustodyVault, SqliteCustodyStore,
};
use nlos_publisher_trust::{
    CrlImportDecision, IntermediateCertificateIssueSpec, LeafCertificateIssueSpec,
    LeafCertificateStatus, OfflinePlatformRootCa, OwnershipTransferProposal,
    PackageReleaseDecision, PublisherId, PublisherKeyRotationReceipt, PublisherLeafCertificate,
    PublisherTrustCheckError, PublisherTrustTier, PublisherTrustVerifier, RevocationEpoch,
    RevocationListSpec, RevocationReason, RevokedCertificateEntry, RotatedKeyDisposition,
    SignedOwnershipTransferReceipt, SignedRevocationList, SqlitePublisherTrustAuthority,
    StoreIntermediateCertificate, ed25519_public_key_from_seed, sign_ed25519_digest_with_seed,
    sign_package_digest_under_custody,
};
use nlos_types::{ApplicationId, ControlDomainId, InstallationId, KeyId, PackageId, PrincipalId};
use rusqlite::Connection;

static NEXT_DIR_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempDirGuard(PathBuf);

impl TempDirGuard {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after UNIX_EPOCH")
            .as_nanos();
        let seq = NEXT_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "nlos-pub-trust-{label}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp test directory");
        Self(dir)
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct PkiFixture {
    _dir: TempDirGuard,
    trust_db_path: PathBuf,
    vault: SigningKeyCustodyVault,
    root_seed: [u8; 32],
    root_ca: OfflinePlatformRootCa,
    platform_ca: StoreIntermediateCertificate,
    certified_ca: StoreIntermediateCertificate,
}

impl PkiFixture {
    fn setup(label: &str) -> (Self, SqlitePublisherTrustAuthority) {
        let dir = TempDirGuard::new(label);
        let vault_dir = dir.0.join("custody");
        let trust_db_path = dir.0.join("publisher-trust.db");

        let custody_store = SqliteCustodyStore::open(&vault_dir).expect("open SqliteCustodyStore");
        let vault =
            SigningKeyCustodyVault::with_store(custody_store, [0xA5; 32]).expect("open KEK vault");

        let root_seed = [0x01; 32];
        let root_ca = OfflinePlatformRootCa::issue_self_signed(
            [0x10; 16],
            KeyId::from_bytes([0x11; 16]),
            &root_seed,
            1_000,
            1_000_000_000,
        )
        .expect("issue offline root CA");

        // Provision PlatformCore-capable Store Intermediate CA inside KEK custody vault.
        let platform_ca_key_id = KeyId::from_bytes([0x21; 16]);
        let platform_ca_slot = vault
            .provision_key_slot(ProvisionCustodyKeyRequest {
                key_id: platform_ca_key_id,
                principal_id: PrincipalId::from_bytes([0x22; 16]),
                control_domain_id: ControlDomainId::from_bytes([0x23; 16]),
                authority_id: [0x24; 16],
                key_purpose: KeyPurpose::SemanticSigning,
                custody_tier: CustodyTier::HardwareHsmAttested,
                rotation_epoch: 1,
                valid_from_ms: 1_000,
                valid_until_ms: 500_000_000,
                seed: [0x02; 32],
                provisioned_at_ms: 1_000,
            })
            .expect("provision platform intermediate CA slot");

        let platform_ca = root_ca
            .issue_intermediate_certificate(
                &root_seed,
                IntermediateCertificateIssueSpec {
                    serial_id: [0x20; 16],
                    intermediate_key_id: platform_ca_key_id,
                    intermediate_public_key: platform_ca_slot.public_key,
                    max_issuable_tier: PublisherTrustTier::PlatformCore,
                    not_before_ms: 1_000,
                    expires_at_ms: 500_000_000,
                },
            )
            .expect("issue platform intermediate CA");

        // Provision CertifiedPublisher-capped Store Intermediate CA inside KEK custody vault.
        let certified_ca_key_id = KeyId::from_bytes([0x31; 16]);
        let certified_ca_slot = vault
            .provision_key_slot(ProvisionCustodyKeyRequest {
                key_id: certified_ca_key_id,
                principal_id: PrincipalId::from_bytes([0x32; 16]),
                control_domain_id: ControlDomainId::from_bytes([0x33; 16]),
                authority_id: [0x34; 16],
                key_purpose: KeyPurpose::SemanticSigning,
                custody_tier: CustodyTier::PlatformKeychainSealed,
                rotation_epoch: 1,
                valid_from_ms: 1_000,
                valid_until_ms: 500_000_000,
                seed: [0x03; 32],
                provisioned_at_ms: 1_000,
            })
            .expect("provision certified intermediate CA slot");

        let certified_ca = root_ca
            .issue_intermediate_certificate(
                &root_seed,
                IntermediateCertificateIssueSpec {
                    serial_id: [0x30; 16],
                    intermediate_key_id: certified_ca_key_id,
                    intermediate_public_key: certified_ca_slot.public_key,
                    max_issuable_tier: PublisherTrustTier::CertifiedPublisher,
                    not_before_ms: 1_000,
                    expires_at_ms: 500_000_000,
                },
            )
            .expect("issue certified intermediate CA");

        let authority =
            SqlitePublisherTrustAuthority::open(&trust_db_path).expect("open trust authority");
        authority
            .anchor_offline_root_ca(&root_ca, 1_500)
            .expect("anchor root CA");
        authority
            .register_intermediate_certificate(&platform_ca, 1_500)
            .expect("register platform intermediate CA");
        authority
            .register_intermediate_certificate(&certified_ca, 1_500)
            .expect("register certified intermediate CA");

        let (initial_crl, _) = SignedRevocationList::issue_with_intermediate_custody(
            &certified_ca,
            &vault,
            Some(1),
            RevocationListSpec {
                revocation_epoch: RevocationEpoch::new(1),
                issued_at_ms: 2_000,
                fresh_until_ms: 60_000,
                revoked_certificates: Vec::new(),
                revoked_publishers: Vec::new(),
            },
        )
        .expect("sign initial CRL under custody");
        assert_eq!(
            authority
                .import_signed_revocation_list(&initial_crl)
                .expect("import initial CRL"),
            CrlImportDecision::Advanced(RevocationEpoch::new(1))
        );

        (
            Self {
                _dir: dir,
                trust_db_path,
                vault,
                root_seed,
                root_ca,
                platform_ca,
                certified_ca,
            },
            authority,
        )
    }
}

#[test]
fn three_tier_pki_issuance_under_kek_custody_and_signature_verification() {
    let (fx, authority) = PkiFixture::setup("three-tier");

    // 1. PlatformCore publisher whose leaf signing key is also sealed in SigningKeyCustodyVault.
    let core_pub = PublisherId::from_bytes([0x41; 16]);
    let core_key_id = KeyId::from_bytes([0x42; 16]);
    let core_slot = fx
        .vault
        .provision_key_slot(ProvisionCustodyKeyRequest {
            key_id: core_key_id,
            principal_id: PrincipalId::from_bytes([0x43; 16]),
            control_domain_id: ControlDomainId::from_bytes([0x44; 16]),
            authority_id: [0x45; 16],
            key_purpose: KeyPurpose::SemanticSigning,
            custody_tier: CustodyTier::OperatorFileSealed,
            rotation_epoch: 1,
            valid_from_ms: 2_000,
            valid_until_ms: 100_000,
            seed: [0x14; 32],
            provisioned_at_ms: 2_000,
        })
        .expect("provision PlatformCore leaf key in custody vault");

    let (core_leaf, _) = fx
        .platform_ca
        .issue_leaf_certificate_under_custody(
            &fx.vault,
            Some(1),
            2_500,
            LeafCertificateIssueSpec {
                serial_id: [0x46; 16],
                publisher_id: core_pub,
                trust_tier: PublisherTrustTier::PlatformCore,
                leaf_key_id: core_key_id,
                leaf_public_key: core_slot.public_key,
                not_before_ms: 2_000,
                expires_at_ms: 100_000,
            },
        )
        .expect("issue PlatformCore leaf cert");
    authority
        .register_leaf_certificate(&core_leaf, 2_500)
        .expect("register PlatformCore leaf cert");

    // 2. CertifiedPublisher leaf issued under CertifiedPublisher intermediate CA.
    let cert_pub = PublisherId::from_bytes([0x51; 16]);
    let cert_seed = [0x15; 32];
    let cert_pk = ed25519_public_key_from_seed(&cert_seed);
    let (cert_leaf, _) = fx
        .certified_ca
        .issue_leaf_certificate_under_custody(
            &fx.vault,
            Some(1),
            2_600,
            LeafCertificateIssueSpec {
                serial_id: [0x52; 16],
                publisher_id: cert_pub,
                trust_tier: PublisherTrustTier::CertifiedPublisher,
                leaf_key_id: KeyId::from_bytes([0x53; 16]),
                leaf_public_key: cert_pk,
                not_before_ms: 2_000,
                expires_at_ms: 40_000,
            },
        )
        .expect("issue CertifiedPublisher leaf cert");
    authority
        .register_leaf_certificate(&cert_leaf, 2_600)
        .expect("register CertifiedPublisher leaf cert");

    // 3. CommunitySandboxed self-signed developer leaf certificate.
    let comm_pub = PublisherId::from_bytes([0x61; 16]);
    let comm_seed = [0x16; 32];
    let comm_leaf = PublisherLeafCertificate::issue_self_signed_community(
        [0x62; 16],
        comm_pub,
        KeyId::from_bytes([0x63; 16]),
        &comm_seed,
        2_000,
        100_000,
    )
    .expect("issue self-signed CommunitySandboxed leaf cert");
    authority
        .register_leaf_certificate(&comm_leaf, 2_700)
        .expect("register CommunitySandboxed leaf cert");

    let pkg_digest = [0xAB; 32];
    let (core_sig, audit) =
        sign_package_digest_under_custody(&fx.vault, core_key_id, Some(1), pkg_digest, 10_000)
            .expect("sign package digest under custody");
    assert!(audit.sign_seq >= 1);

    let verified_core = authority
        .verify_publisher_signature(
            core_pub,
            &core_slot.public_key,
            &pkg_digest,
            &core_sig,
            10_000,
        )
        .expect("verify PlatformCore signature");
    assert_eq!(verified_core.trust_tier, PublisherTrustTier::PlatformCore);
    assert_eq!(verified_core.revocation_epoch, RevocationEpoch::new(1));

    let cert_sig = sign_ed25519_digest_with_seed(&cert_seed, &pkg_digest);
    let verified_cert = authority
        .verify_publisher_signature(cert_pub, &cert_pk, &pkg_digest, &cert_sig, 10_000)
        .expect("verify CertifiedPublisher signature");
    assert_eq!(
        verified_cert.trust_tier,
        PublisherTrustTier::CertifiedPublisher
    );

    let comm_sig = sign_ed25519_digest_with_seed(&comm_seed, &pkg_digest);
    let verified_comm = authority
        .verify_publisher_signature(
            comm_pub,
            &comm_leaf.leaf_public_key,
            &pkg_digest,
            &comm_sig,
            10_000,
        )
        .expect("verify CommunitySandboxed signature");
    assert_eq!(
        verified_comm.trust_tier,
        PublisherTrustTier::CommunitySandboxed
    );

    // Tampered digest fails with SignatureInvalid.
    let mut bad_digest = pkg_digest;
    bad_digest[0] ^= 0xFF;
    assert_eq!(
        authority.verify_publisher_signature(cert_pub, &cert_pk, &bad_digest, &cert_sig, 10_000),
        Err(PublisherTrustCheckError::SignatureInvalid)
    );

    // Expired leaf certificate fails with ExpiredCertificate.
    assert_eq!(
        authority.verify_publisher_signature(cert_pub, &cert_pk, &pkg_digest, &cert_sig, 45_000),
        Err(PublisherTrustCheckError::ExpiredCertificate {
            expires_at_ms: 40_000,
            now_ms: 45_000,
        })
    );

    // Unknown publisher fails with UnknownPublisher.
    let ghost = PublisherId::from_bytes([0xFE; 16]);
    assert_eq!(
        authority.verify_publisher_signature(ghost, &cert_pk, &pkg_digest, &cert_sig, 10_000),
        Err(PublisherTrustCheckError::UnknownPublisher(ghost))
    );
}

#[test]
fn forged_intermediate_and_cross_tier_privilege_escalation_fail_closed() {
    let (fx, authority) = PkiFixture::setup("escalation");

    // 1. Rogue unanchored root CA forging an intermediate CA -> UntrustedCertificateChain.
    let rogue_root_seed = [0x99; 32];
    let rogue_root = OfflinePlatformRootCa::issue_self_signed(
        [0x90; 16],
        KeyId::from_bytes([0x91; 16]),
        &rogue_root_seed,
        1_000,
        500_000,
    )
    .expect("rogue root");
    let forged_intermediate = rogue_root
        .issue_intermediate_certificate(
            &rogue_root_seed,
            IntermediateCertificateIssueSpec {
                serial_id: [0x92; 16],
                intermediate_key_id: KeyId::from_bytes([0x93; 16]),
                intermediate_public_key: ed25519_public_key_from_seed(&[0x94; 32]),
                max_issuable_tier: PublisherTrustTier::PlatformCore,
                not_before_ms: 1_000,
                expires_at_ms: 400_000,
            },
        )
        .expect("forged intermediate");
    assert!(matches!(
        authority.register_intermediate_certificate(&forged_intermediate, 3_000),
        Err(PublisherTrustCheckError::UntrustedCertificateChain(_))
    ));

    // 2. Self-signed developer key claiming PlatformCore or CertifiedPublisher -> UntrustedCertificateChain.
    let attacker_pub = PublisherId::from_bytes([0x71; 16]);
    let attacker_seed = [0x72; 32];
    let mut escalated_self_signed = PublisherLeafCertificate::issue_self_signed_community(
        [0x73; 16],
        attacker_pub,
        KeyId::from_bytes([0x74; 16]),
        &attacker_seed,
        2_000,
        100_000,
    )
    .expect("valid community cert");
    for forbidden_tier in [
        PublisherTrustTier::PlatformCore,
        PublisherTrustTier::CertifiedPublisher,
    ] {
        escalated_self_signed.trust_tier = forbidden_tier;
        let digest = escalated_self_signed.signing_digest();
        escalated_self_signed.signature = sign_ed25519_digest_with_seed(&attacker_seed, &digest);
        assert!(matches!(
            authority.register_leaf_certificate(&escalated_self_signed, 3_000),
            Err(PublisherTrustCheckError::UntrustedCertificateChain(_))
        ));
    }

    // 3. CertifiedPublisher-capped intermediate CA attempting to issue PlatformCore -> UntrustedCertificateChain.
    let cross_tier_attempt = fx.certified_ca.issue_leaf_certificate_under_custody(
        &fx.vault,
        Some(1),
        3_000,
        LeafCertificateIssueSpec {
            serial_id: [0x75; 16],
            publisher_id: attacker_pub,
            trust_tier: PublisherTrustTier::PlatformCore,
            leaf_key_id: KeyId::from_bytes([0x76; 16]),
            leaf_public_key: ed25519_public_key_from_seed(&attacker_seed),
            not_before_ms: 2_000,
            expires_at_ms: 100_000,
        },
    );
    assert!(matches!(
        cross_tier_attempt,
        Err(PublisherTrustCheckError::UntrustedCertificateChain(_))
    ));
}

#[test]
fn key_rotation_preserves_application_ownership_and_compromised_old_key_invalidates() {
    let (fx, authority) = PkiFixture::setup("rotation");

    let pub_id = PublisherId::from_bytes([0xA1; 16]);
    let app_id = ApplicationId::from_bytes([0xB1; 16]);
    let seed_k1 = [0x11; 32];
    let seed_k2 = [0x22; 32];
    let seed_k3 = [0x33; 32];

    let (leaf_k1, _) = fx
        .certified_ca
        .issue_leaf_certificate_under_custody(
            &fx.vault,
            Some(1),
            3_000,
            LeafCertificateIssueSpec {
                serial_id: [0xC1; 16],
                publisher_id: pub_id,
                trust_tier: PublisherTrustTier::CertifiedPublisher,
                leaf_key_id: KeyId::from_bytes([0xD1; 16]),
                leaf_public_key: ed25519_public_key_from_seed(&seed_k1),
                not_before_ms: 2_000,
                expires_at_ms: 200_000,
            },
        )
        .expect("issue k1");
    authority
        .register_leaf_certificate(&leaf_k1, 3_000)
        .expect("register k1");

    // Initial ownership binding with k1.
    let initial_owner = authority
        .verify_or_bind_application_owner(
            pub_id,
            PublisherTrustTier::CertifiedPublisher,
            app_id,
            3_500,
        )
        .expect("bind app owner under k1");
    assert_eq!(initial_owner.registration_generation, 1);

    // Planned dual-signed rotation k1 -> k2 (RetireVerifyOnly).
    let (leaf_k2, _) = fx
        .certified_ca
        .issue_leaf_certificate_under_custody(
            &fx.vault,
            Some(1),
            4_000,
            LeafCertificateIssueSpec {
                serial_id: [0xC2; 16],
                publisher_id: pub_id,
                trust_tier: PublisherTrustTier::CertifiedPublisher,
                leaf_key_id: KeyId::from_bytes([0xD2; 16]),
                leaf_public_key: ed25519_public_key_from_seed(&seed_k2),
                not_before_ms: 4_000,
                expires_at_ms: 200_000,
            },
        )
        .expect("issue k2");
    let rot_1_to_2 = PublisherKeyRotationReceipt::issue_dual_signed(
        &leaf_k1,
        &seed_k1,
        leaf_k2,
        &seed_k2,
        RotatedKeyDisposition::RetireVerifyOnly,
        4_500,
    );
    authority
        .rotate_publisher_key(&rot_1_to_2)
        .expect("rotate k1 -> k2");
    assert_eq!(
        authority
            .inspect_leaf_certificate_status(leaf_k1.serial_id)
            .expect("inspect k1 status"),
        Some(LeafCertificateStatus::RotatedRetired)
    );

    // k2 signs new release and seamlessly inherits `app_id` ownership!
    let digest_v2 = [0xE2; 32];
    let sig_v2 = sign_ed25519_digest_with_seed(&seed_k2, &digest_v2);
    let verified_k2 = authority
        .verify_publisher_signature(pub_id, &leaf_k2.leaf_public_key, &digest_v2, &sig_v2, 5_000)
        .expect("k2 verifies");
    assert_eq!(verified_k2.leaf_key_id, leaf_k2.leaf_key_id);
    let inherited = authority
        .verify_or_bind_application_owner(
            pub_id,
            PublisherTrustTier::CertifiedPublisher,
            app_id,
            5_000,
        )
        .expect("k2 inherits app_id ownership");
    assert_eq!(inherited, initial_owner);

    // Emergency Store-Intermediate-CA custody-attested rotation k2 -> k3 due to k2 compromise!
    let (leaf_k3, _) = fx
        .certified_ca
        .issue_leaf_certificate_under_custody(
            &fx.vault,
            Some(1),
            6_000,
            LeafCertificateIssueSpec {
                serial_id: [0xC3; 16],
                publisher_id: pub_id,
                trust_tier: PublisherTrustTier::CertifiedPublisher,
                leaf_key_id: KeyId::from_bytes([0xD3; 16]),
                leaf_public_key: ed25519_public_key_from_seed(&seed_k3),
                not_before_ms: 6_000,
                expires_at_ms: 200_000,
            },
        )
        .expect("issue k3");
    let (rot_2_to_3, _) = PublisherKeyRotationReceipt::issue_intermediate_attested_under_custody(
        &leaf_k2,
        leaf_k3,
        &seed_k3,
        &fx.certified_ca,
        &fx.vault,
        Some(1),
        RotatedKeyDisposition::RevokeCompromised {
            revoked_at_epoch: RevocationEpoch::new(1),
        },
        6_500,
    )
    .expect("issue intermediate-attested rotation k2 -> k3");
    authority
        .rotate_publisher_key(&rot_2_to_3)
        .expect("rotate k2 -> k3 and mark k2 compromised");
    assert_eq!(
        authority
            .inspect_leaf_certificate_status(leaf_k2.serial_id)
            .expect("inspect k2 status"),
        Some(LeafCertificateStatus::Compromised)
    );

    // Compromised k2 IMMEDIATELY fails closed in both online and cached offline launch modes!
    assert!(matches!(
        authority.verify_publisher_signature(
            pub_id,
            &leaf_k2.leaf_public_key,
            &digest_v2,
            &sig_v2,
            7_000,
        ),
        Err(PublisherTrustCheckError::RevokedCertificate { .. })
    ));
    assert!(matches!(
        authority.verify_cached_offline_launch(
            pub_id,
            &leaf_k2.leaf_public_key,
            &digest_v2,
            &sig_v2,
            7_000,
        ),
        Err(PublisherTrustCheckError::RevokedCertificate { .. })
    ));

    // k3 works and preserves app_id ownership.
    let digest_v3 = [0xE3; 32];
    let sig_v3 = sign_ed25519_digest_with_seed(&seed_k3, &digest_v3);
    authority
        .verify_publisher_signature(pub_id, &leaf_k3.leaf_public_key, &digest_v3, &sig_v3, 7_000)
        .expect("k3 verifies cleanly");
    assert_eq!(
        authority
            .verify_or_bind_application_owner(
                pub_id,
                PublisherTrustTier::CertifiedPublisher,
                app_id,
                7_000,
            )
            .expect("k3 still owns app_id"),
        initial_owner
    );
}

#[test]
fn crl_revocation_epoch_anti_rollback_equivocation_and_whole_publisher_revocation() {
    let (fx, authority) = PkiFixture::setup("crl-rollback");

    let pub_a = PublisherId::from_bytes([0x81; 16]);
    let seed_a = [0x82; 32];
    let leaf_a = PublisherLeafCertificate::issue_self_signed_community(
        [0x83; 16],
        pub_a,
        KeyId::from_bytes([0x84; 16]),
        &seed_a,
        2_000,
        200_000,
    )
    .expect("leaf a");
    authority
        .register_leaf_certificate(&leaf_a, 3_000)
        .expect("register leaf a");

    let pub_b = PublisherId::from_bytes([0x85; 16]);
    let seed_b = [0x86; 32];
    let leaf_b = PublisherLeafCertificate::issue_self_signed_community(
        [0x87; 16],
        pub_b,
        KeyId::from_bytes([0x88; 16]),
        &seed_b,
        2_000,
        200_000,
    )
    .expect("leaf b");
    authority
        .register_leaf_certificate(&leaf_b, 3_000)
        .expect("register leaf b");

    // Advance CRL to epoch 3, revoking leaf_a by serial and pub_b as a whole publisher.
    let (crl_v3, _) = SignedRevocationList::issue_with_intermediate_custody(
        &fx.certified_ca,
        &fx.vault,
        Some(1),
        RevocationListSpec {
            revocation_epoch: RevocationEpoch::new(3),
            issued_at_ms: 10_000,
            fresh_until_ms: 80_000,
            revoked_certificates: vec![RevokedCertificateEntry {
                serial_or_key: leaf_a.serial_id,
                reason: RevocationReason::MaliciousPackage,
            }],
            revoked_publishers: vec![pub_b],
        },
    )
    .expect("sign CRL epoch 3");
    assert_eq!(
        authority
            .import_signed_revocation_list(&crl_v3)
            .expect("import CRL epoch 3"),
        CrlImportDecision::Advanced(RevocationEpoch::new(3))
    );
    assert_eq!(
        authority
            .import_signed_revocation_list(&crl_v3)
            .expect("idempotent replay CRL epoch 3"),
        CrlImportDecision::Replayed(RevocationEpoch::new(3))
    );
    assert_eq!(
        authority.current_revocation_epoch(),
        Ok(RevocationEpoch::new(3))
    );

    // Rollback attack: signed CRL at epoch 2 MUST fail closed!
    let (rollback_crl_v2, _) = SignedRevocationList::issue_with_intermediate_custody(
        &fx.certified_ca,
        &fx.vault,
        Some(1),
        RevocationListSpec {
            revocation_epoch: RevocationEpoch::new(2),
            issued_at_ms: 11_000,
            fresh_until_ms: 90_000,
            revoked_certificates: Vec::new(),
            revoked_publishers: Vec::new(),
        },
    )
    .expect("sign rollback CRL epoch 2");
    assert_eq!(
        authority.import_signed_revocation_list(&rollback_crl_v2),
        Err(PublisherTrustCheckError::StaleOrRollbackRevocationEpoch {
            current_epoch: RevocationEpoch::new(3),
            candidate_epoch: RevocationEpoch::new(2),
        })
    );

    // Equivocation attack: different CRL content at same epoch 3 MUST fail closed!
    let (equivocated_crl_v3, _) = SignedRevocationList::issue_with_intermediate_custody(
        &fx.certified_ca,
        &fx.vault,
        Some(1),
        RevocationListSpec {
            revocation_epoch: RevocationEpoch::new(3),
            issued_at_ms: 10_000,
            fresh_until_ms: 80_000,
            revoked_certificates: Vec::new(),
            revoked_publishers: Vec::new(),
        },
    )
    .expect("sign equivocated CRL epoch 3");
    assert_eq!(
        authority.import_signed_revocation_list(&equivocated_crl_v3),
        Err(PublisherTrustCheckError::StaleOrRollbackRevocationEpoch {
            current_epoch: RevocationEpoch::new(3),
            candidate_epoch: RevocationEpoch::new(3),
        })
    );

    // Verify revocations take immediate effect.
    let msg = [0x55; 32];
    let sig_a = sign_ed25519_digest_with_seed(&seed_a, &msg);
    assert_eq!(
        authority.verify_publisher_signature(pub_a, &leaf_a.leaf_public_key, &msg, &sig_a, 15_000),
        Err(PublisherTrustCheckError::RevokedCertificate {
            serial_or_key: leaf_a.serial_id,
            revoked_at_epoch: RevocationEpoch::new(3),
        })
    );
    let sig_b = sign_ed25519_digest_with_seed(&seed_b, &msg);
    assert_eq!(
        authority.verify_publisher_signature(pub_b, &leaf_b.leaf_public_key, &msg, &sig_b, 15_000),
        Err(PublisherTrustCheckError::RevokedPublisher {
            publisher_id: pub_b,
            revoked_at_epoch: RevocationEpoch::new(3),
        })
    );
}

#[test]
fn crl_anti_freeze_online_block_vs_dui_offline_001_cached_installed_launch() {
    let (fx, authority) = PkiFixture::setup("anti-freeze-offline");

    let pub_id = PublisherId::from_bytes([0x9A; 16]);
    let seed = [0x9B; 32];
    let pk = ed25519_public_key_from_seed(&seed);
    let (leaf, _) = fx
        .certified_ca
        .issue_leaf_certificate_under_custody(
            &fx.vault,
            Some(1),
            3_000,
            LeafCertificateIssueSpec {
                serial_id: [0x9C; 16],
                publisher_id: pub_id,
                trust_tier: PublisherTrustTier::CertifiedPublisher,
                leaf_key_id: KeyId::from_bytes([0x9D; 16]),
                leaf_public_key: pk,
                not_before_ms: 2_000,
                expires_at_ms: 500_000,
            },
        )
        .expect("issue leaf");
    authority
        .register_leaf_certificate(&leaf, 3_000)
        .expect("register leaf");

    let app_digest = [0x77; 32];
    let app_sig = sign_ed25519_digest_with_seed(&seed, &app_digest);

    // Inside initial CRL freshness window (fresh_until_ms = 60_000): online install passes.
    authority
        .verify_publisher_signature(pub_id, &pk, &app_digest, &app_sig, 50_000)
        .expect("online install passes while CRL is fresh");

    // Beyond CRL freshness window (now_ms = 120_000 > 60_000):
    // 1. Online install/upgrade MUST fail closed with FrozenRevocationList (anti-freeze).
    assert_eq!(
        authority.verify_publisher_signature(pub_id, &pk, &app_digest, &app_sig, 120_000),
        Err(PublisherTrustCheckError::FrozenRevocationList {
            expires_at_ms: 60_000,
            now_ms: 120_000,
        })
    );
    // 2. Local already-installed offline launch MUST succeed ([DUI-OFFLINE-001]).
    let offline_ok = authority
        .verify_cached_offline_launch(pub_id, &pk, &app_digest, &app_sig, 120_000)
        .expect("offline launch of installed unrevoked app succeeds under DUI-OFFLINE-001");
    assert_eq!(offline_ok.publisher_id, pub_id);

    // 3. Once a fresh CRL (epoch 2) revoking this certificate arrives, offline launch blocks immediately!
    let root_crl_v2 = SignedRevocationList::issue_with_root_seed(
        &fx.root_ca,
        &fx.root_seed,
        RevocationListSpec {
            revocation_epoch: RevocationEpoch::new(2),
            issued_at_ms: 125_000,
            fresh_until_ms: 200_000,
            revoked_certificates: vec![RevokedCertificateEntry {
                serial_or_key: leaf.serial_id,
                reason: RevocationReason::KeyCompromise,
            }],
            revoked_publishers: Vec::new(),
        },
    )
    .expect("issue root CRL v2");
    authority
        .import_signed_revocation_list(&root_crl_v2)
        .expect("import CRL v2");

    assert_eq!(
        authority.verify_cached_offline_launch(pub_id, &pk, &app_digest, &app_sig, 250_000),
        Err(PublisherTrustCheckError::RevokedCertificate {
            serial_or_key: leaf.serial_id,
            revoked_at_epoch: RevocationEpoch::new(2),
        })
    );
}

#[test]
fn application_id_first_registration_pkg_sep_001_evolution_and_hijack_interception() {
    let (fx, authority) = PkiFixture::setup("ownership-pkg-sep");

    let owner_pub = PublisherId::from_bytes([0xA8; 16]);
    let owner_seed = [0xA9; 32];
    let (owner_leaf, _) = fx
        .certified_ca
        .issue_leaf_certificate_under_custody(
            &fx.vault,
            Some(1),
            3_000,
            LeafCertificateIssueSpec {
                serial_id: [0xAA; 16],
                publisher_id: owner_pub,
                trust_tier: PublisherTrustTier::CertifiedPublisher,
                leaf_key_id: KeyId::from_bytes([0xAB; 16]),
                leaf_public_key: ed25519_public_key_from_seed(&owner_seed),
                not_before_ms: 2_000,
                expires_at_ms: 300_000,
            },
        )
        .expect("issue owner leaf");
    authority
        .register_leaf_certificate(&owner_leaf, 3_000)
        .expect("register owner leaf");

    let rival_pub = PublisherId::from_bytes([0xAC; 16]);
    let rival_leaf = PublisherLeafCertificate::issue_self_signed_community(
        [0xAD; 16],
        rival_pub,
        KeyId::from_bytes([0xAE; 16]),
        &[0xAF; 32],
        2_000,
        300_000,
    )
    .expect("issue rival leaf");
    authority
        .register_leaf_certificate(&rival_leaf, 3_000)
        .expect("register rival leaf");

    let app_id = ApplicationId::from_bytes([0xB0; 16]);
    let pkg_v1 = PackageId::from_bytes([0xB1; 16]);
    let pkg_v2 = PackageId::from_bytes([0xB2; 16]);
    let inst_1 = InstallationId::from_bytes([0xB3; 16]);
    let inst_2 = InstallationId::from_bytes([0xB4; 16]);

    // First registration + multi-PackageId evolution under stable ApplicationId.
    let (own_1, rel_1) = authority
        .bind_package_release_for_application(
            pkg_v1,
            app_id,
            owner_pub,
            PublisherTrustTier::CertifiedPublisher,
            [0x11; 32],
            5_000,
        )
        .expect("bind pkg_v1");
    assert!(matches!(rel_1, PackageReleaseDecision::Bound(_)));
    assert_eq!(own_1.first_registered_at_ms, 5_000);
    assert_eq!(own_1.registration_generation, 1);

    let (own_2, rel_2) = authority
        .bind_package_release_for_application(
            pkg_v2,
            app_id,
            owner_pub,
            PublisherTrustTier::CertifiedPublisher,
            [0x22; 32],
            9_000,
        )
        .expect("bind pkg_v2 to same app_id");
    assert!(matches!(rel_2, PackageReleaseDecision::Bound(_)));
    assert_eq!(own_2, own_1);
    assert_eq!(
        authority
            .list_application_package_releases(app_id)
            .expect("list releases")
            .len(),
        2
    );

    // Multiple distinct InstallationIds derived cleanly ([PKG-SEP-001]).
    authority
        .verify_installation_identity_separation(pkg_v1, app_id, inst_1)
        .expect("inst_1 separated");
    authority
        .verify_installation_identity_separation(pkg_v2, app_id, inst_2)
        .expect("inst_2 separated");

    // [PKG-SEP-001] violations: aliasing PackageId == ApplicationId or InstallationId == ApplicationId.
    let aliased_pkg = PackageId::from_bytes(*app_id.as_bytes());
    assert!(matches!(
        authority.bind_package_release_for_application(
            aliased_pkg,
            app_id,
            owner_pub,
            PublisherTrustTier::CertifiedPublisher,
            [0x33; 32],
            9_500,
        ),
        Err(PublisherTrustCheckError::StorageFailure(_))
    ));
    let aliased_inst = InstallationId::from_bytes(*app_id.as_bytes());
    assert!(matches!(
        authority.verify_installation_identity_separation(pkg_v1, app_id, aliased_inst),
        Err(PublisherTrustCheckError::StorageFailure(_))
    ));

    // [PKG-IMM-001] violation: mutating an already-published PackageId's manifest digest fails closed.
    assert!(matches!(
        authority.bind_package_release_for_application(
            pkg_v1,
            app_id,
            owner_pub,
            PublisherTrustTier::CertifiedPublisher,
            [0xFF; 32],
            9_600,
        ),
        Err(PublisherTrustCheckError::StorageFailure(_))
    ));

    // [PKG-APPID-001] Hijack interception for both registered rival and unregistered attacker!
    assert_eq!(
        authority.verify_or_bind_application_owner(
            rival_pub,
            PublisherTrustTier::CommunitySandboxed,
            app_id,
            10_000,
        ),
        Err(PublisherTrustCheckError::ApplicationIdHijackAttempt {
            application_id: app_id,
            registered_owner: owner_pub,
            attempted_publisher: rival_pub,
        })
    );
    let unregistered_hijacker = PublisherId::from_bytes([0xEE; 16]);
    assert_eq!(
        authority.verify_or_bind_application_owner(
            unregistered_hijacker,
            PublisherTrustTier::CommunitySandboxed,
            app_id,
            10_000,
        ),
        Err(PublisherTrustCheckError::ApplicationIdHijackAttempt {
            application_id: app_id,
            registered_owner: owner_pub,
            attempted_publisher: unregistered_hijacker,
        })
    );
}

#[test]
fn three_party_cosigned_application_ownership_transfer_and_sqlite_restart_triggers() {
    let (fx, authority) = PkiFixture::setup("transfer-and-triggers");

    let pub_old = PublisherId::from_bytes([0xD1; 16]);
    let seed_old = [0xD2; 32];
    let pk_old = ed25519_public_key_from_seed(&seed_old);
    let (leaf_old, _) = fx
        .certified_ca
        .issue_leaf_certificate_under_custody(
            &fx.vault,
            Some(1),
            3_000,
            LeafCertificateIssueSpec {
                serial_id: [0xD3; 16],
                publisher_id: pub_old,
                trust_tier: PublisherTrustTier::CertifiedPublisher,
                leaf_key_id: KeyId::from_bytes([0xD4; 16]),
                leaf_public_key: pk_old,
                not_before_ms: 2_000,
                expires_at_ms: 300_000,
            },
        )
        .expect("issue old owner leaf");
    authority
        .register_leaf_certificate(&leaf_old, 3_000)
        .expect("register old owner leaf");

    let pub_new = PublisherId::from_bytes([0xE1; 16]);
    let seed_new = [0xE2; 32];
    let pk_new = ed25519_public_key_from_seed(&seed_new);
    let (leaf_new, _) = fx
        .certified_ca
        .issue_leaf_certificate_under_custody(
            &fx.vault,
            Some(1),
            3_100,
            LeafCertificateIssueSpec {
                serial_id: [0xE3; 16],
                publisher_id: pub_new,
                trust_tier: PublisherTrustTier::CertifiedPublisher,
                leaf_key_id: KeyId::from_bytes([0xE4; 16]),
                leaf_public_key: pk_new,
                not_before_ms: 2_000,
                expires_at_ms: 300_000,
            },
        )
        .expect("issue new owner leaf");
    authority
        .register_leaf_certificate(&leaf_new, 3_100)
        .expect("register new owner leaf");

    let app_id = ApplicationId::from_bytes([0xF1; 16]);
    let gen1 = authority
        .verify_or_bind_application_owner(
            pub_old,
            PublisherTrustTier::CertifiedPublisher,
            app_id,
            5_000,
        )
        .expect("bind gen 1");
    assert_eq!(gen1.registration_generation, 1);

    let proposal = OwnershipTransferProposal {
        application_id: app_id,
        expected_current_generation: 1,
        from_publisher_id: pub_old,
        from_leaf_public_key: pk_old,
        to_publisher_id: pub_new,
        to_trust_tier: PublisherTrustTier::CertifiedPublisher,
        to_leaf_public_key: pk_new,
        intermediate_serial_id: fx.certified_ca.serial_id,
        transferred_at_ms: 12_000,
    };

    // Negative: forged Store CA signature (2-party attempt without Store Intermediate CA) fails closed!
    let forged_ca_receipt = SignedOwnershipTransferReceipt::sign_three_party_with_seeds(
        proposal,
        &seed_old,
        &seed_new,
        &[0x99; 32],
    );
    assert_eq!(
        authority.transfer_application_ownership(&forged_ca_receipt),
        Err(PublisherTrustCheckError::SignatureInvalid)
    );
    assert_eq!(authority.inspect_application_owner(app_id), Ok(Some(gen1)));

    // Valid 3-party co-signed receipt (Store Intermediate CA co-signs under KEK custody vault).
    let (valid_transfer, _) = SignedOwnershipTransferReceipt::sign_three_party_under_store_custody(
        proposal,
        &seed_old,
        &seed_new,
        &fx.certified_ca,
        &fx.vault,
        Some(1),
    )
    .expect("3-party sign under store custody");

    let gen2 = authority
        .transfer_application_ownership(&valid_transfer)
        .expect("atomic 3-party ownership transfer");
    assert_eq!(gen2.owner_publisher_id, pub_new);
    assert_eq!(gen2.first_registered_at_ms, 5_000);
    assert_eq!(gen2.registration_generation, 2);

    // Former owner can no longer bind or publish under `app_id`!
    assert_eq!(
        authority.verify_or_bind_application_owner(
            pub_old,
            PublisherTrustTier::CertifiedPublisher,
            app_id,
            13_000,
        ),
        Err(PublisherTrustCheckError::ApplicationIdHijackAttempt {
            application_id: app_id,
            registered_owner: pub_new,
            attempted_publisher: pub_old,
        })
    );

    // Close authority, verify raw SQLite WAL/FULL/triggers, then reopen and verify durability!
    drop(authority);

    let raw = Connection::open(&fx.trust_db_path).expect("open raw SQLite connection");
    let user_version: i64 = raw
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read user_version");
    assert_eq!(user_version, 1);

    // Anti-rollback trigger on `trust_revocation_state` MUST abort epoch downgrade!
    assert!(
        raw.execute(
            "UPDATE trust_revocation_state SET revocation_epoch = 0 WHERE singleton_id = 1",
            [],
        )
        .is_err()
    );
    // Anti-hijack trigger on `application_ownership_bindings` MUST abort direct owner rewrite!
    assert!(
        raw.execute(
            "UPDATE application_ownership_bindings
             SET owner_publisher_id = ?2, registration_generation = 3
             WHERE application_id = ?1",
            rusqlite::params![app_id.as_bytes().as_slice(), pub_old.as_bytes().as_slice()],
        )
        .is_err()
    );
    // Immutable transfer receipt table MUST abort UPDATE and DELETE!
    assert!(
        raw.execute("DELETE FROM application_ownership_transfers", [],)
            .is_err()
    );
    let integrity: String = raw
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("integrity_check");
    assert_eq!(integrity, "ok");
    drop(raw);

    let reopened =
        SqlitePublisherTrustAuthority::open(&fx.trust_db_path).expect("reopen trust authority");
    assert_eq!(
        reopened.current_revocation_epoch(),
        Ok(RevocationEpoch::new(1))
    );
    assert_eq!(reopened.inspect_application_owner(app_id), Ok(Some(gen2)));
    assert_eq!(
        reopened
            .inspect_ownership_transfers(app_id)
            .expect("inspect transfer audit trail"),
        vec![valid_transfer]
    );
}
