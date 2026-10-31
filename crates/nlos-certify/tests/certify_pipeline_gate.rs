//! Stage E `E5` (`E-CERTIFY-001`) integration gate suite for `nlos-certify`:
//! Gate 1..Gate 4 conformance, pure-Rust dependency SPDX/lockfile supply-chain
//! auditing (`RISK-B-09`), `StoreCertificationReceipt` custody signing and
//! verification, and CLI exit-code determinism.

#![forbid(unsafe_code)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use ed25519_dalek::SigningKey;
use nlos_capability::{CustodyTier, ProvisionCustodyKeyRequest, SigningKeyCustodyVault};
use nlos_certify::json::parse_json;
use nlos_certify::{
    AdvisoryDenyEntry, CertifyError, CertifyPipelineConfig, DependencyEcosystem, Gate3DrillOptions,
    Gate4DrillOptions, GateId, GateStatus, IssueStoreCertificationRequest, LoadedCertifyPackage,
    PublisherSubmissionProof, StaticPublisherTrustVerifier, StoreCertificationReceipt,
    StoreCertifierSigner, SupplyChainPolicy, SupplyChainViolationKind, SurfaceProfile,
    audit_dependency_supply_chain, derive_publisher_id_from_str, evaluate_spdx_expression,
    issue_store_certification_receipt, run_certification_pipeline, run_gate1_protocol_audit,
    run_gate2_supply_chain_audit, run_gate3_update_rollback_drill_with_options,
    run_gate4_uninstall_audit_with_options, verify_store_certification_receipt,
};
use nlos_identity::KeyPurpose;
use nlos_publisher_trust::{
    PublisherId, PublisherTrustTier, RevocationEpoch, VerifiedPublisherIdentity,
};
use nlos_types::{ControlDomainId, KeyId, PrincipalId};

static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempFixtureDir {
    path: PathBuf,
}

impl TempFixtureDir {
    fn new(label: &str) -> Self {
        let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = env::temp_dir().join(format!("nlos-certify-test-{label}-{pid}-{seq}"));
        if path.exists() {
            let _ = fs::remove_dir_all(&path);
        }
        fs::create_dir_all(&path).expect("create temp fixture dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write_file(&self, rel: &str, contents: &[u8]) {
        let full = self.path.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("create fixture parent dir");
        }
        fs::write(full, contents).expect("write fixture file");
    }
}

impl Drop for TempFixtureDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn build_custody_vault_certifier() -> (SigningKeyCustodyVault, KeyId, [u8; 32]) {
    let vault = SigningKeyCustodyVault::new_in_memory([0x5A_u8; 32]).expect("open custody vault");
    let key_id = KeyId::from_bytes([0xCE_u8; 16]);
    let slot = vault
        .provision_key_slot(ProvisionCustodyKeyRequest {
            key_id,
            principal_id: PrincipalId::from_bytes([0xCF_u8; 16]),
            control_domain_id: ControlDomainId::from_bytes([0xCD_u8; 16]),
            authority_id: [0xCA_u8; 16],
            key_purpose: KeyPurpose::SemanticSigning,
            custody_tier: CustodyTier::OperatorFileSealed,
            rotation_epoch: 1,
            valid_from_ms: 1_000,
            valid_until_ms: 9_000_000_000_000,
            seed: [0x39_u8; 32],
            provisioned_at_ms: 1_000,
        })
        .expect("provision certifier key slot");
    (vault, key_id, slot.public_key)
}

fn seed_clean_stage_e_package(fixture: &TempFixtureDir, extra_manifest_lines: &str) {
    let manifest = format!(
        "\
package-id = 10101010101010101010101010101010
version = 1.0.0
target-sabi-epoch = 3
driver-abi-epoch = 1
migration-mode = SNAPSHOT_ASSISTED_ROLLBACK
component = worker-exec executable bin/worker.bin
component = sync-svc background-service bin/sync.bin
component = storage-drv driver bin/storage.drv
component = app-seed seed-template assets/seed.json
effect = worker-exec READ_ONLY reconcile=same-idempotency-key
effect = sync-svc REVERSIBLE reconcile=same-idempotency-key
effect = storage-drv::write COMPENSATABLE reconcile=same-idempotency-key
effect = storage-drv::commit IRREVERSIBLE reconcile=same-idempotency-key
idempotency-passthrough = worker-exec 128-bit
idempotency-passthrough = storage-drv 128-bit
sabi-service = import ARTIFACT feature=scoped-chooser,content-addressed
sabi-service = import TASK feature=background-job,cancel-fence
chooser-scope = artifact-chooser
default-association = suggest
{extra_manifest_lines}
"
    );
    fixture.write_file("package.manifest", manifest.as_bytes());
    fixture.write_file("bin/worker.bin", b"nlos-worker-v1");
    fixture.write_file("bin/sync.bin", b"nlos-sync-v1");
    fixture.write_file("bin/storage.drv", b"nlos-storage-drv-v1");
    fixture.write_file("assets/seed.json", b"{\"schema\":1,\"mode\":\"clean\"}\n");
}

#[test]
fn starter_assistant_app_v1_to_v1_1_passes_all_four_gates_and_issues_signed_receipt() {
    let root = repo_root();
    let v1_dir = root.join("examples/starter-assistant-app/package");
    let v2_dir = root.join("examples/starter-assistant-app/package-v1.1.0");

    let loaded_v1 = LoadedCertifyPackage::load(&v1_dir).expect("load starter v1");
    let publisher_id = derive_publisher_id_from_str("nlos.first-party.starter");
    let publisher_sk = SigningKey::from_bytes(&[0x41_u8; 32]);
    let submission_proof =
        PublisherSubmissionProof::for_package(publisher_id, &publisher_sk, &loaded_v1);
    let issued_at_ms = 1_761_868_800_000;
    let trust_verifier = StaticPublisherTrustVerifier::single_verified_owner(
        publisher_id,
        PublisherTrustTier::PlatformCore,
        publisher_sk.verifying_key().to_bytes(),
        loaded_v1.application_id,
        RevocationEpoch::new(3),
        issued_at_ms,
    );

    let (vault, key_id, certifier_pk) = build_custody_vault_certifier();
    let signer = StoreCertifierSigner::CustodyVault {
        vault: &vault,
        key_id,
        expected_rotation_epoch: Some(1),
    };
    let config = CertifyPipelineConfig::new(
        submission_proof,
        SurfaceProfile::ApplicationPlatform,
        issued_at_ms,
    );

    let report =
        run_certification_pipeline(&v1_dir, Some(&v2_dir), &config, &trust_verifier, &signer)
            .expect("run 4-gate certification pipeline");

    assert!(
        report.all_gates_passed(),
        "starter-assistant-app must pass 4/4 gates: {}",
        report.render_text()
    );
    assert!(report.certified());
    assert_eq!(report.gate3.healthy_activated_generation, Some(2));
    assert_eq!(
        report.gate3.baseline_container_tree_digest,
        report.gate3.health_fail_restored_tree_digest
    );
    assert_eq!(
        report.gate3.baseline_container_tree_digest,
        report.gate3.mid_step_fault_restored_tree_digest
    );
    assert!(report.gate4.active_task_guard_verified);
    assert_eq!(report.gate4.revoked_capabilities_verified, 2);
    assert_eq!(report.gate4.residual_private_cache_files, 0);
    assert!(report.gate4.shared_artifact_preserved);
    assert!(report.gate4.user_data_preserved_by_default_and_z4_gated);

    let receipt = report
        .receipt
        .as_ref()
        .expect("issued StoreCertificationReceipt");
    verify_store_certification_receipt(receipt, Some(&certifier_pk), issued_at_ms)
        .expect("verify StoreCertificationReceipt");

    // Round-trip through canonical RFC 8259 JSON.
    let json_wire = receipt.to_json();
    let decoded = StoreCertificationReceipt::from_json(&json_wire).expect("decode receipt JSON");
    assert_eq!(&decoded, receipt);
    verify_store_certification_receipt(&decoded, Some(&certifier_pk), issued_at_ms + 1_000)
        .expect("verify round-tripped JSON receipt");
}

#[test]
fn gate1_enforces_surface_profile_catalog_and_settlement_escrow_negotiation() {
    // 1. Clean headless APPLICATION_PLATFORM package with SEMANTIC_SETTLEMENT + RESOURCE_ESCROW passes.
    let clean_dir = TempFixtureDir::new("gate1-clean");
    seed_clean_stage_e_package(
        &clean_dir,
        "sabi-service = import RESOURCE_ESCROW feature=quota-hold\nsabi-service = import SEMANTIC_SETTLEMENT feature=two-phase-settle",
    );
    let clean_pkg = LoadedCertifyPackage::load(clean_dir.path()).expect("load clean");
    let clean_out = run_gate1_protocol_audit(&clean_pkg, SurfaceProfile::ApplicationPlatform, 3)
        .expect("run gate 1");
    assert_eq!(clean_out.summary.status, GateStatus::Pass);

    // 2. Desktop-exclusive service (`UI_SURFACE`) + `ui-surface` component rejected under APPLICATION_PLATFORM,
    //    but accepted under DESKTOP profile!
    let desktop_dir = TempFixtureDir::new("gate1-desktop");
    seed_clean_stage_e_package(
        &desktop_dir,
        "component = main-window ui-surface assets/seed.json\nsabi-service = import UI_SURFACE feature=declarative-view",
    );
    let desktop_pkg = LoadedCertifyPackage::load(desktop_dir.path()).expect("load desktop pkg");
    let app_prof_out =
        run_gate1_protocol_audit(&desktop_pkg, SurfaceProfile::ApplicationPlatform, 3)
            .expect("audit under APPLICATION_PLATFORM");
    assert_eq!(app_prof_out.summary.status, GateStatus::Fail);
    assert!(
        app_prof_out
            .findings
            .iter()
            .any(|f| f.rule_id == "SABI-CONFORM-001"),
        "expected SABI-CONFORM-001 blocker"
    );

    let desktop_prof_out = run_gate1_protocol_audit(&desktop_pkg, SurfaceProfile::Desktop, 3)
        .expect("audit under DESKTOP");
    assert_eq!(desktop_prof_out.summary.status, GateStatus::Pass);

    // 3. SEMANTIC_SETTLEMENT without RESOURCE_ESCROW is rejected with SABI-CONFORM-001 BLOCKER.
    let no_escrow_dir = TempFixtureDir::new("gate1-no-escrow");
    seed_clean_stage_e_package(
        &no_escrow_dir,
        "sabi-service = import SEMANTIC_SETTLEMENT feature=two-phase-settle",
    );
    let no_escrow_pkg = LoadedCertifyPackage::load(no_escrow_dir.path()).expect("load no-escrow");
    let no_escrow_out =
        run_gate1_protocol_audit(&no_escrow_pkg, SurfaceProfile::ApplicationPlatform, 3)
            .expect("audit no-escrow");
    assert_eq!(no_escrow_out.summary.status, GateStatus::Fail);
    assert!(
        no_escrow_out
            .findings
            .iter()
            .any(|f| { f.rule_id == "SABI-CONFORM-001" && f.message.contains("RESOURCE_ESCROW") })
    );
}

#[test]
fn gate1_rejects_epoch_window_unknown_critical_extension_missing_effects_and_bad_reconcile() {
    let bad_dir = TempFixtureDir::new("gate1-violations");
    bad_dir.write_file(
        "package.manifest",
        b"\
package-id = 20202020202020202020202020202020
version = 1.0.0
target-sabi-epoch = 9
sabi-ext = nlos.sabi.Envelope critical 9999
component = naked-svc background-service bin/svc.bin
component = mut-exec executable bin/mut.bin
effect = mut-exec REVERSIBLE reconcile=none
idempotency-passthrough = mut-exec drop
",
    );
    bad_dir.write_file("bin/svc.bin", b"svc");
    bad_dir.write_file("bin/mut.bin", b"mut");

    let pkg = LoadedCertifyPackage::load(bad_dir.path()).expect("load bad protocol pkg");
    let out =
        run_gate1_protocol_audit(&pkg, SurfaceProfile::ApplicationPlatform, 3).expect("run gate 1");
    assert_eq!(out.summary.status, GateStatus::Fail);

    let rules: Vec<&str> = out.findings.iter().map(|f| f.rule_id.as_str()).collect();
    assert!(
        rules.contains(&"COMPAT-VER-001"),
        "missing COMPAT-VER-001: {rules:?}"
    );
    assert!(
        rules.contains(&"COMPAT-VER-002"),
        "missing COMPAT-VER-002: {rules:?}"
    );
    assert!(
        rules.contains(&"DRV-LIFE-001"),
        "missing DRV-LIFE-001: {rules:?}"
    );
    assert!(
        rules.contains(&"KABI-UNCERTAIN-001"),
        "missing KABI-UNCERTAIN-001: {rules:?}"
    );
}

#[test]
fn gate2_enforces_egress_chooser_association_sqlite_purity_and_secret_redaction() {
    // 1. Approved declared HTTPS egress passes Gate 2.
    let declared_egress_dir = TempFixtureDir::new("gate2-declared-egress");
    seed_clean_stage_e_package(
        &declared_egress_dir,
        "network-policy = egress-declared\negress-allow = https://api.trusted-partner.example/v1",
    );
    declared_egress_dir.write_file(
        "assets/endpoint.txt",
        b"upstream=https://api.trusted-partner.example/v1/ inference\n",
    );
    let ok_pkg = LoadedCertifyPackage::load(declared_egress_dir.path()).expect("load ok egress");
    let ok_out =
        run_gate2_supply_chain_audit(&ok_pkg, &SupplyChainPolicy::default()).expect("gate 2 ok");
    assert_eq!(ok_out.summary.status, GateStatus::Pass);

    // 2. Violations: cloud metadata IP, undeclared external URL, raw host path, forced file association,
    //    baked-in SQLite DB, and hardcoded API secret (which MUST be redacted in report!).
    let bad_dir = TempFixtureDir::new("gate2-violations");
    seed_clean_stage_e_package(
        &bad_dir,
        "default-association = force\nchooser-scope = raw-host-path",
    );
    let raw_secret_tail = "SUPERSECRET9876543210ZYXWVU";
    let secret_payload = format!(
        "metadata_probe=http://169.254.169.254/latest/meta-data/\nhost_target=C:\\Windows\\System32\\drivers\\etc\\hosts\napi_key = \"sk-live-{raw_secret_tail}\"\n"
    );
    bad_dir.write_file("assets/evil-config.txt", secret_payload.as_bytes());
    bad_dir.write_file("assets/baked.sqlite", b"SQLite format 3\0mutable-state");

    let bad_pkg = LoadedCertifyPackage::load(bad_dir.path()).expect("load bad gate2 pkg");
    let bad_out =
        run_gate2_supply_chain_audit(&bad_pkg, &SupplyChainPolicy::default()).expect("gate 2 bad");
    assert_eq!(bad_out.summary.status, GateStatus::Fail);

    let rules: Vec<&str> = bad_out
        .findings
        .iter()
        .map(|f| f.rule_id.as_str())
        .collect();
    assert!(
        rules.contains(&"TCB-EGRESS-001"),
        "missing TCB-EGRESS-001: {rules:?}"
    );
    assert!(
        rules.contains(&"DUI-CHOOSER-001"),
        "missing DUI-CHOOSER-001: {rules:?}"
    );
    assert!(
        rules.contains(&"DUI-ASSOC-001"),
        "missing DUI-ASSOC-001: {rules:?}"
    );
    assert!(
        rules.contains(&"PKG-IMM-001"),
        "missing PKG-IMM-001: {rules:?}"
    );
    assert!(
        rules.contains(&"PKG-SEED-001"),
        "missing PKG-SEED-001: {rules:?}"
    );

    for finding in &bad_out.findings {
        assert!(
            !finding.message.contains(raw_secret_tail),
            "Gate 2 leaked plaintext secret into finding message: {}",
            finding.message
        );
    }
}

#[test]
fn supply_chain_audit_passes_on_real_repository_workspace_cargo_and_npm_lockfiles() {
    let root = repo_root();
    let policy = SupplyChainPolicy::default();

    let ws_report = audit_dependency_supply_chain(&root, &policy)
        .expect("audit real repository workspace root");
    assert!(
        ws_report.passed(),
        "real repository supply-chain audit must have 0 blockers:\n{}",
        ws_report.render_text()
    );
    assert!(
        ws_report.audited_cargo_packages >= 100,
        "expected >= 100 audited Cargo packages, got {}",
        ws_report.audited_cargo_packages
    );
    assert!(
        ws_report.audited_npm_packages >= 100,
        "expected >= 100 audited npm packages across root + desktop, got {}",
        ws_report.audited_npm_packages
    );

    let desktop_report = audit_dependency_supply_chain(&root.join("desktop"), &policy)
        .expect("audit desktop npm root directly");
    assert!(
        desktop_report.passed(),
        "desktop npm supply-chain audit must have 0 blockers:\n{}",
        desktop_report.render_text()
    );
    assert!(desktop_report.audited_npm_packages >= 50);
}

#[test]
fn supply_chain_audit_blocks_copyleft_missing_checksum_floating_versions_and_advisories() {
    let policy = SupplyChainPolicy::default().with_advisory_deny(AdvisoryDenyEntry {
        ecosystem: DependencyEcosystem::Cargo,
        package_name: "vuln-crate".to_owned(),
        version: "0.4.2".to_owned(),
        advisory_id: "RUSTSEC-2026-9999".to_owned(),
        reason: "use-after-free in parser".to_owned(),
    });

    // Verify SPDX expression boolean AST semantics directly.
    assert!(evaluate_spdx_expression(
        "(MIT OR Apache-2.0) AND Unicode-3.0",
        &policy
    ));
    assert!(evaluate_spdx_expression(
        "Apache-2.0 WITH LLVM-exception OR MIT",
        &policy
    ));
    assert!(evaluate_spdx_expression("MIT OR GPL-3.0-only", &policy));
    assert!(!evaluate_spdx_expression("MIT AND GPL-3.0-only", &policy));
    assert!(!evaluate_spdx_expression("AGPL-3.0-or-later", &policy));
    assert!(!evaluate_spdx_expression("UNLICENSED", &policy));

    let bad_sc = TempFixtureDir::new("supply-chain-violations");
    bad_sc.write_file(
        "Cargo.toml",
        b"\
[package]
name = \"violating-pkg\"
version = \"0.1.0\"
license = \"GPL-3.0-only\"

[dependencies]
floating-dep = \"*\"
unpinned-git = { git = \"https://github.com/example/unpinned.git\" }
",
    );
    bad_sc.write_file(
        "Cargo.lock",
        b"\
version = 4

[[package]]
name = \"no-cksum-crate\"
version = \"1.2.3\"
source = \"registry+https://github.com/rust-lang/crates.io-index\"
license = \"MIT\"

[[package]]
name = \"vuln-crate\"
version = \"0.4.2\"
source = \"registry+https://github.com/rust-lang/crates.io-index\"
checksum = \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"
license = \"Apache-2.0\"

[[package]]
name = \"missing-lic-crate\"
version = \"0.1.0\"
source = \"registry+https://github.com/rust-lang/crates.io-index\"
checksum = \"fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210\"
license = \"\"
",
    );
    bad_sc.write_file(
        "package.json",
        b"{\"name\":\"bad-web\",\"version\":\"1.0.0\",\"license\":\"MIT\",\"dependencies\":{\"wild-js\":\"*\"}}",
    );
    // Intentionally omit `package-lock.json` to trigger LockfileMissing as well.

    let report =
        audit_dependency_supply_chain(bad_sc.path(), &policy).expect("audit violating fixture");
    assert!(!report.passed());
    let kinds: Vec<SupplyChainViolationKind> = report.findings.iter().map(|f| f.kind).collect();
    assert!(kinds.contains(&SupplyChainViolationKind::DisallowedLicense));
    assert!(kinds.contains(&SupplyChainViolationKind::FloatingDependency));
    assert!(kinds.contains(&SupplyChainViolationKind::MissingChecksum));
    assert!(kinds.contains(&SupplyChainViolationKind::AdvisoryDenied));
    assert!(kinds.contains(&SupplyChainViolationKind::MissingLicense));
    assert!(kinds.contains(&SupplyChainViolationKind::LockfileMissing));
    assert!(report.findings.iter().all(|f| f.rule_id == "RISK-B-09"));
}

#[test]
fn gate3_rejects_irreversible_mode_and_catches_simulated_rollback_drift() {
    let irrev_dir = TempFixtureDir::new("gate3-irreversible");
    irrev_dir.write_file(
        "package.manifest",
        b"\
package-id = irrev.pkg
version = 1.0.0
target-sabi-epoch = 3
migration-mode = IRREVERSIBLE
component = worker executable bin/worker.bin
effect = worker REVERSIBLE reconcile=same-idempotency-key
",
    );
    irrev_dir.write_file("bin/worker.bin", b"v1");
    let irrev_pkg = LoadedCertifyPackage::load(irrev_dir.path()).expect("load irrev pkg");
    let irrev_out = run_gate3_update_rollback_drill_with_options(
        &irrev_pkg,
        None,
        Gate3DrillOptions::default(),
    )
    .expect("run gate3 on irreversible pkg");
    assert_eq!(irrev_out.summary.status, GateStatus::Fail);
    assert!(
        irrev_out
            .findings
            .iter()
            .any(|f| f.rule_id == "PKG-UPDATE-001")
    );

    // Clean package fails Gate 3 when simulated rollback container drift is injected.
    let clean_dir = TempFixtureDir::new("gate3-drift");
    seed_clean_stage_e_package(&clean_dir, "");
    let clean_pkg = LoadedCertifyPackage::load(clean_dir.path()).expect("load clean pkg");
    let drift_out = run_gate3_update_rollback_drill_with_options(
        &clean_pkg,
        None,
        Gate3DrillOptions {
            simulate_unclean_rollback_drift: true,
        },
    )
    .expect("run gate3 with simulated drift");
    assert_eq!(drift_out.summary.status, GateStatus::Fail);
    assert!(
        drift_out
            .findings
            .iter()
            .any(|f| f.rule_id == "PKG-UPDATE-001")
    );
}

#[test]
fn gate4_catches_unrevoked_capabilities_residual_cache_and_unconsented_user_data_purge() {
    let clean_dir = TempFixtureDir::new("gate4-faults");
    seed_clean_stage_e_package(&clean_dir, "");
    let pkg = LoadedCertifyPackage::load(clean_dir.path()).expect("load clean pkg");

    let unrevoked = run_gate4_uninstall_audit_with_options(
        &pkg,
        Gate4DrillOptions {
            simulate_unrevoked_capability: true,
            ..Gate4DrillOptions::default()
        },
    )
    .expect("run gate4 unrevoked");
    assert_eq!(unrevoked.summary.status, GateStatus::Fail);
    assert!(
        unrevoked
            .findings
            .iter()
            .any(|f| f.rule_id == "CAP-REVOKE-001")
    );

    let cache_leak = run_gate4_uninstall_audit_with_options(
        &pkg,
        Gate4DrillOptions {
            simulate_residual_private_cache: true,
            ..Gate4DrillOptions::default()
        },
    )
    .expect("run gate4 cache leak");
    assert_eq!(cache_leak.summary.status, GateStatus::Fail);
    assert!(
        cache_leak
            .findings
            .iter()
            .any(|f| f.rule_id == "PKG-UNINSTALL-001" && f.subject == "gate4/private-cache")
    );

    let user_data_loss = run_gate4_uninstall_audit_with_options(
        &pkg,
        Gate4DrillOptions {
            simulate_unconsented_user_data_deletion: true,
            ..Gate4DrillOptions::default()
        },
    )
    .expect("run gate4 user data loss");
    assert_eq!(user_data_loss.summary.status, GateStatus::Fail);
    assert!(
        user_data_loss
            .findings
            .iter()
            .any(|f| f.rule_id == "PKG-UNINSTALL-001" && f.subject == "gate4/user-data-container")
    );
}

#[test]
fn receipt_issuance_enforces_publisher_ownership_all_gates_pass_and_tamper_resistance() {
    let clean_dir = TempFixtureDir::new("receipt-security");
    seed_clean_stage_e_package(&clean_dir, "");
    let pkg = LoadedCertifyPackage::load(clean_dir.path()).expect("load pkg");

    let owner_pub = PublisherId::from_bytes([0x11_u8; 16]);
    let owner_sk = SigningKey::from_bytes(&[0x71_u8; 32]);
    let attacker_pub = PublisherId::from_bytes([0x22_u8; 16]);
    let attacker_sk = SigningKey::from_bytes(&[0x72_u8; 32]);
    let issued_at_ms = 100_000;

    let mut verifier = StaticPublisherTrustVerifier::single_verified_owner(
        owner_pub,
        PublisherTrustTier::CertifiedPublisher,
        owner_sk.verifying_key().to_bytes(),
        pkg.application_id,
        RevocationEpoch::new(7),
        issued_at_ms,
    );
    verifier.register_publisher(VerifiedPublisherIdentity {
        publisher_id: attacker_pub,
        trust_tier: PublisherTrustTier::CommunitySandboxed,
        leaf_key_id: KeyId::from_bytes([0xD2_u8; 16]),
        leaf_public_key: attacker_sk.verifying_key().to_bytes(),
        revocation_epoch: RevocationEpoch::new(1),
        verified_at_ms: issued_at_ms,
    });

    let ed_key = SigningKey::from_bytes(&[0x88_u8; 32]);
    let ed_pk = ed_key.verifying_key().to_bytes();
    let signer = StoreCertifierSigner::Ed25519Key {
        key_id: KeyId::from_bytes([0xE1_u8; 16]),
        signing_key: &ed_key,
    };

    // 1. Attacker has valid publisher signature, but does NOT own `pkg.application_id` ->
    //    pipeline refuses receipt issuance with ApplicationIdHijackAttempt!
    let attacker_proof = PublisherSubmissionProof::for_package(attacker_pub, &attacker_sk, &pkg);
    let attacker_cfg = CertifyPipelineConfig::new(
        attacker_proof,
        SurfaceProfile::ApplicationPlatform,
        issued_at_ms,
    );
    let attacker_rep =
        run_certification_pipeline(clean_dir.path(), None, &attacker_cfg, &verifier, &signer)
            .expect("run pipeline as non-owner");
    assert!(attacker_rep.all_gates_passed());
    assert!(!attacker_rep.certified());
    assert!(attacker_rep.receipt.is_none());

    // 2. Legitimate owner -> receipt issued and verifies cleanly.
    let owner_proof = PublisherSubmissionProof::for_package(owner_pub, &owner_sk, &pkg);
    let owner_cfg = CertifyPipelineConfig::new(
        owner_proof.clone(),
        SurfaceProfile::ApplicationPlatform,
        issued_at_ms,
    );
    let owner_rep =
        run_certification_pipeline(clean_dir.path(), None, &owner_cfg, &verifier, &signer)
            .expect("run pipeline as legitimate owner");
    let receipt = owner_rep.receipt.expect("owner receipt issued");
    verify_store_certification_receipt(&receipt, Some(&ed_pk), issued_at_ms).expect("valid");

    // 3. Single failed gate refuses `issue_store_certification_receipt`.
    let mut failing_summaries = receipt.gate_summaries.clone();
    failing_summaries[2].status = GateStatus::Fail;
    failing_summaries[2].blocker_count = 1;
    let refused = issue_store_certification_receipt(
        &verifier,
        &signer,
        IssueStoreCertificationRequest {
            submission_proof: owner_proof,
            application_id: pkg.application_id,
            package_id: pkg.package_id,
            package_version: pkg.base_manifest.version_raw.clone(),
            manifest_digest: pkg.manifest_digest,
            surface_profile: SurfaceProfile::ApplicationPlatform,
            target_sabi_epoch: 3,
            gate_summaries: failing_summaries,
            issued_at_ms,
            expires_at_ms: issued_at_ms + 60_000,
        },
    );
    assert!(matches!(
        refused,
        Err(CertifyError::GatesNotAllPassed { ref failed_gates })
            if failed_gates == &[GateId::Gate3TransactionalUpdateAndRollback]
    ));

    // 4. Single-bit signature tamper, version tamper, gate status tamper, wrong public key, and expiry all fail closed!
    let mut sig_tampered = receipt.clone();
    sig_tampered.ed25519_signature[63] ^= 0x01;
    assert!(verify_store_certification_receipt(&sig_tampered, Some(&ed_pk), issued_at_ms).is_err());

    let mut ver_tampered = receipt.clone();
    ver_tampered.package_version = "1.0.1".to_owned();
    assert!(verify_store_certification_receipt(&ver_tampered, Some(&ed_pk), issued_at_ms).is_err());

    let mut gate_tampered = receipt.clone();
    gate_tampered.gate_summaries[3].status = GateStatus::Fail;
    assert!(
        verify_store_certification_receipt(&gate_tampered, Some(&ed_pk), issued_at_ms).is_err()
    );

    let wrong_pk = [0xAA_u8; 32];
    assert!(verify_store_certification_receipt(&receipt, Some(&wrong_pk), issued_at_ms).is_err());
    assert!(
        verify_store_certification_receipt(&receipt, Some(&ed_pk), receipt.expires_at_ms + 1)
            .is_err()
    );
}

#[test]
fn cli_binary_certify_and_supply_chain_subcommands_return_deterministic_exit_codes() {
    let bin = env!("CARGO_BIN_EXE_nlos-certify");
    let root = repo_root();

    let help_out = Command::new(bin)
        .arg("--help")
        .output()
        .expect("invoke nlos-certify --help");
    assert!(help_out.status.success());
    let help_stdout = String::from_utf8_lossy(&help_out.stdout);
    assert!(help_stdout.contains("certify"));
    assert!(help_stdout.contains("supply-chain"));

    let certify_out = Command::new(bin)
        .args([
            "certify",
            "--package-dir",
            root.join("examples/starter-assistant-app/package")
                .to_str()
                .expect("utf8 path"),
            "--upgrade-dir",
            root.join("examples/starter-assistant-app/package-v1.1.0")
                .to_str()
                .expect("utf8 path"),
            "--profile",
            "application-platform",
            "--format",
            "json",
        ])
        .output()
        .expect("invoke nlos-certify certify");
    assert!(
        certify_out.status.success(),
        "CLI certify failed: stderr={}",
        String::from_utf8_lossy(&certify_out.stderr)
    );
    let parsed_cert =
        parse_json(&String::from_utf8_lossy(&certify_out.stdout)).expect("valid certify JSON");
    assert_eq!(
        parsed_cert
            .as_object()
            .and_then(|o| o.get("verdict"))
            .and_then(|v| v.as_str()),
        Some("CERTIFIED")
    );

    let sc_out = Command::new(bin)
        .args([
            "supply-chain",
            "--root",
            root.to_str().expect("utf8 root"),
            "--format",
            "json",
        ])
        .output()
        .expect("invoke nlos-certify supply-chain");
    assert!(
        sc_out.status.success(),
        "CLI supply-chain failed: stderr={}",
        String::from_utf8_lossy(&sc_out.stderr)
    );
    let parsed_sc =
        parse_json(&String::from_utf8_lossy(&sc_out.stdout)).expect("valid supply-chain JSON");
    assert_eq!(
        parsed_sc
            .as_object()
            .and_then(|o| o.get("verdict"))
            .and_then(|v| v.as_str()),
        Some("PASS")
    );

    // Violating package directory must exit with status code 2.
    let bad_dir = TempFixtureDir::new("cli-blocked");
    bad_dir.write_file(
        "package.manifest",
        b"package-id = 30303030303030303030303030303030\nversion = 1.0.0\ntarget-sabi-epoch = 99\ncomponent = dummy executable bin/d.bin\neffect = dummy READ_ONLY\n",
    );
    bad_dir.write_file("bin/d.bin", b"dummy");
    let blocked_out = Command::new(bin)
        .args([
            "certify",
            "--package-dir",
            bad_dir.path().to_str().expect("utf8 path"),
        ])
        .output()
        .expect("invoke nlos-certify on blocked pkg");
    assert_eq!(blocked_out.status.code(), Some(2));
}
