//! Integration tests for `nlos-migrate` static scanner and `UpgradePrescription`
//! compiler (`Stage E W78 / E3`).

#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ed25519_dalek::{Signer, SigningKey};
use nlos_application::pack_package_version;
use nlos_artifact::{
    PackageEntryRole, PackageFile, PackageFileEntry, SignerDescriptor, derive_artifact_id,
    package_manifest_message,
};
use nlos_migrate::{
    ExecutionPosture, FindingCategory, FindingSeverity, ManifestLoadError, RemediationKind,
    RouteClassification, ScanError, ScanOptions, TargetSourceKind, UpgradePrescription, scan_path,
};
use nlos_types::PackageId;
use sha2::{Digest, Sha256};

static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempTestDir {
    path: PathBuf,
}

impl TempTestDir {
    fn new(tag: &str) -> Self {
        let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-migrate-scan-{tag}-{}-{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp test dir");
        Self { path }
    }
}

impl Drop for TempTestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn evolved_manifest_yaml() -> &'static str {
    "\
manifest_version: 1
current_epoch: 4
min_compatibility_window_epochs: 2
frozen_channels_total: 9
channels:
  - name: nlos.sabi.Envelope
    major: 1
    minor: 1
    frozen: true
    reserved_field_numbers: [90]
    reserved_symbols: [\"Envelope.legacy_auth_blob\"]
  - name: nlos.sabi.ServiceDirectory
    major: 1
    minor: 0
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []
  - name: nlos.sabi.OperationControl
    major: 1
    minor: 0
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []
  - name: nlos.sabi.SystemControl
    major: 1
    minor: 7
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []
  - name: nlos.sabi.TakeoverControl
    major: 1
    minor: 0
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []
  - name: nlos.sabi.WaitControl
    major: 1
    minor: 0
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []
  - name: nlos.sabi.PrincipalHandshake
    major: 1
    minor: 0
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []
  - name: nlos.sabi.DriverProvider
    major: 1
    minor: 0
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []
  - name: nlos.sabi.ChannelControl
    major: 1
    minor: 0
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []
entries:
  - channel: nlos.sabi.Envelope
    symbol: SabiRequestContext.correlation_id
    kind: FIELD
    parent_type: SabiRequestContext
    field_number: 4
    introduced_in_epoch: 1
    state: ACTIVE
  - channel: nlos.sabi.OperationControl
    symbol: OperationControlRequest.legacy_timeout_ms
    kind: FIELD
    parent_type: OperationControlRequest
    field_number: 12
    introduced_in_epoch: 1
    state: DEPRECATED
    deprecated_in_epoch: 3
    earliest_sunset_epoch: 5
    replacement: SabiRequestContext.deadline_monotonic_ns
    migration_rule_id: MIG-OP-TIMEOUT-001
    reason: Replaced by monotonic deadline in request context
  - channel: nlos.sabi.DriverProvider
    symbol: DriverHealthPingV0
    kind: METHOD
    introduced_in_epoch: 1
    state: SUNSET
    deprecated_in_epoch: 2
    earliest_sunset_epoch: 4
    sunset_in_epoch: 4
    replacement: DriverHealthProbeV1
    migration_rule_id: MIG-DRV-HEALTH-001
    reason: Retired after two-epoch grace window
"
}

fn write_source_pkg(dir: &Path, pkg_manifest: &str, mig_manifest: Option<&str>) {
    fs::create_dir_all(dir).expect("mkdir pkg");
    fs::write(dir.join("package.manifest"), pkg_manifest).expect("write package.manifest");
    if let Some(mig) = mig_manifest {
        fs::write(dir.join("migration.manifest"), mig).expect("write migration.manifest");
    }
}

#[test]
fn canonical_sample_app_directory_scans_clean_and_native_ready() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let sample_dir = repo_root.join("examples/sample-app/package");
    let opts = ScanOptions::canonical_for_host_epoch(1).expect("canonical options");
    let report = scan_path(&sample_dir, &opts).expect("scan sample-app");

    assert!(
        report.passed(),
        "unexpected findings: {:?}",
        report.findings
    );
    assert_eq!(report.target.source_kind, TargetSourceKind::SourceDirectory);
    assert_eq!(report.channel_routes.len(), 1);
    assert_eq!(
        report.channel_routes[0].route,
        RouteClassification::NativeCurrentEpoch
    );

    let prescription = UpgradePrescription::from_scan_report(&report);
    assert_eq!(prescription.posture, ExecutionPosture::NativeCurrentReady);
    assert!(!prescription.compat_shim_transient_allowed);
    assert_eq!(prescription.actions.len(), 0);
}

#[test]
fn grace_window_deprecation_and_n_minus_1_shim_emit_structured_prescription() {
    let tmp = TempTestDir::new("grace-shim");
    write_source_pkg(
        &tmp.path,
        "package-id = 11223344556677889900aabbccddeeff\nversion = 1.2.0\nentry = main executable app.bin\n",
        Some(
            "target-sabi-epoch = 3\n\
             target-sabi-minor = 1\n\
             driver-abi-epoch = 3\n\
             non-critical-extensions = 701,702\n\
             channel = nlos.sabi.Envelope 3.1 non-critical=701,702\n\
             channel = nlos.sabi.OperationControl 3.0\n\
             symbol = nlos.sabi.Envelope SabiRequestContext.correlation_id field=4\n\
             symbol = nlos.sabi.OperationControl OperationControlRequest.legacy_timeout_ms field=12\n\
             effect = read-profile READ_ONLY\n\
             effect = write-draft COMPENSATABLE compensation=revert_draft_v1\n\
             dependency = model qwen-coder@gen:3\n\
             data-migration-mode = BACKWARD_COMPATIBLE_WINDOW\n\
             migration-step = ALTER TABLE drafts ADD COLUMN tag TEXT NOT NULL DEFAULT '';\n",
        ),
    );

    let opts = ScanOptions::from_manifest_yaml(Some(4), evolved_manifest_yaml()).expect("opts");
    let report = scan_path(&tmp.path, &opts).expect("scan grace pkg");

    assert!(
        report.passed(),
        "expected grace window pass: {:?}",
        report.findings
    );
    assert_eq!(report.active_symbols_verified, 1);
    assert_eq!(report.deprecations.len(), 1);
    let dep = &report.deprecations[0];
    assert_eq!(dep.deprecated_in_epoch, 3);
    assert_eq!(dep.earliest_sunset_epoch, 5);
    assert_eq!(dep.remaining_grace_epochs, 1);
    assert_eq!(
        dep.replacement_symbol,
        "SabiRequestContext.deadline_monotonic_ns"
    );
    assert_eq!(dep.migration_rule_id.as_deref(), Some("MIG-OP-TIMEOUT-001"));
    assert!(report.uses_compat_shim());

    let prescription = UpgradePrescription::from_scan_report(&report);
    assert_eq!(
        prescription.posture,
        ExecutionPosture::TransientCompatShimEligible
    );
    assert!(prescription.compat_shim_transient_allowed);
    assert_eq!(prescription.min_remaining_grace_epochs, Some(1));
    assert!(
        prescription
            .actions
            .iter()
            .any(|act| act.kind == RemediationKind::ReplaceContractSymbol
                && act.deadline_epoch == Some(5))
    );
    assert_eq!(report.render_json(), report.render_json());
    assert_eq!(prescription.render_json(), prescription.render_json());
}

#[test]
fn sunset_reserved_critical_extension_and_floating_selector_block_admission() {
    let tmp = TempTestDir::new("blocking");
    write_source_pkg(
        &tmp.path,
        "package-id = aabbccddeeff00112233445566778899\nversion = 2.0.0\nentry = main executable app.bin\n",
        Some(
            "target-sabi-epoch = 1\n\
             critical-extensions = 999\n\
             channel = nlos.sabi.Envelope 1.0 critical=999\n\
             symbol = nlos.sabi.DriverProvider DriverHealthPingV0\n\
             symbol = nlos.sabi.Envelope Envelope.legacy_auth_blob field=90\n\
             effect = broken-comp COMPENSATABLE\n\
             dependency = package foo-helper:latest\n",
        ),
    );

    let opts = ScanOptions::from_manifest_yaml(Some(4), evolved_manifest_yaml()).expect("opts");
    let report = scan_path(&tmp.path, &opts).expect("scan blocked pkg");

    assert!(!report.passed());
    assert!(!report.has_disguised_irreversible());
    let codes: Vec<&str> = report
        .findings
        .iter()
        .filter(|f| f.severity == FindingSeverity::BlockingError)
        .map(|f| f.code)
        .collect();
    assert!(codes.contains(&"E_SUNSET_EPOCH"), "codes={codes:?}");
    assert!(
        codes.contains(&"E_SYMBOL_SUNSET_OR_RESERVED"),
        "codes={codes:?}"
    );
    assert!(
        codes.contains(&"E_COMPENSATABLE_MISSING_HANDLER"),
        "codes={codes:?}"
    );
    assert!(
        codes.contains(&"E_FLOATING_DEPENDENCY_SELECTOR"),
        "codes={codes:?}"
    );

    let prescription = UpgradePrescription::from_scan_report(&report);
    assert_eq!(
        prescription.posture,
        ExecutionPosture::SourceMigrationRequired
    );
    assert!(!prescription.compat_shim_transient_allowed);
    assert!(
        prescription
            .actions
            .iter()
            .all(|a| a.mandatory_before_admission)
    );
}

#[test]
fn unknown_critical_extension_blocks_unless_host_declares_support() {
    let tmp = TempTestDir::new("crit-ext");
    write_source_pkg(
        &tmp.path,
        "package-id = aabbccddeeff00112233445566778899\nversion = 1.0.0\nentry = main executable app.bin\n",
        Some("target-sabi-epoch = 1\ncritical-extensions = 42\n"),
    );

    let default_opts = ScanOptions::canonical_for_host_epoch(1).expect("opts");
    let blocked = scan_path(&tmp.path, &default_opts).expect("scan blocked");
    assert!(!blocked.passed());
    assert!(
        blocked
            .findings
            .iter()
            .any(|f| f.code == "E_UNKNOWN_CRITICAL_EXTENSION")
    );

    let mut supported_opts = default_opts;
    supported_opts.known_critical_extensions = vec![42];
    let allowed = scan_path(&tmp.path, &supported_opts).expect("scan allowed");
    assert!(allowed.passed());
}

#[test]
fn static_scanner_flags_disguised_irreversible_sql_under_window_or_snapshot_modes() {
    let tmp = TempTestDir::new("disguised");
    write_source_pkg(
        &tmp.path,
        "package-id = aabbccddeeff00112233445566778899\nversion = 1.1.0\nentry = main executable app.bin\n",
        Some(
            "data-migration-mode = SNAPSHOT_ASSISTED_ROLLBACK\n\
             migration-step = ALTER TABLE notes DROP COLUMN legacy_body;\n",
        ),
    );
    let opts = ScanOptions::canonical_for_host_epoch(1).expect("opts");
    let report = scan_path(&tmp.path, &opts).expect("scan disguised");
    assert!(!report.passed());
    assert!(report.has_disguised_irreversible());
    assert!(report.findings.iter().any(|f| {
        f.code == "E_DISGUISED_IRREVERSIBLE_MIGRATION"
            && f.category == FindingCategory::DisguisedIrreversibleMigration
    }));
}

#[test]
fn built_nlospkg_archive_scans_clean_and_tampered_nlospkg_fails_closed() {
    let tmp = TempTestDir::new("nlospkg");
    let signing_key = SigningKey::from_bytes(&[0x91; 32]);
    let package_id = PackageId::from_bytes([0x44; 16]);
    let version = pack_package_version(1, 3, 0);
    let mig_payload =
        b"target-sabi-epoch = 1\neffect = sync-op REVERSIBLE\ndependency = artifact abcd@gen:2\n"
            .to_vec();
    let mut hasher = Sha256::new();
    hasher.update(&mig_payload);
    let mig_digest: [u8; 32] = hasher.finalize().into();

    let mut pkg = PackageFile {
        descriptor: SignerDescriptor {
            public_key: signing_key.verifying_key().to_bytes(),
            profile_digest: [0x10; 32],
            policy_digest: [0x20; 32],
            bootstrap_key: [0x30; 16],
            valid_from_ms: 1_000,
            valid_until_ms: 5_000_000,
        },
        package_id,
        version,
        entries: vec![PackageFileEntry {
            name: "migration.manifest".to_owned(),
            role: PackageEntryRole::Data,
            artifact_id: derive_artifact_id(package_id, version, "migration.manifest"),
            digest: mig_digest,
            payload: mig_payload,
        }],
        tasks: Vec::new(),
        signature: [0u8; 64],
    };
    pkg.signature = signing_key
        .sign(&package_manifest_message(&pkg.manifest()))
        .to_bytes();

    let pkg_path = tmp.path.join("valid.nlospkg");
    let encoded = pkg.encode();
    fs::write(&pkg_path, &encoded).expect("write nlospkg");

    let opts = ScanOptions::canonical_for_host_epoch(1).expect("opts");
    let report = scan_path(&pkg_path, &opts).expect("scan valid nlospkg");
    assert!(report.passed());
    assert_eq!(
        report.target.source_kind,
        TargetSourceKind::BuiltPackageFile
    );
    assert_eq!(report.target.version, (1, 3, 0));

    let mut tampered = encoded;
    let last_idx = tampered.len() - 1;
    tampered[last_idx] ^= 0xFF;
    let tampered_path = tmp.path.join("tampered.nlospkg");
    fs::write(&tampered_path, &tampered).expect("write tampered");
    let err = scan_path(&tampered_path, &opts).expect_err("tampered must fail");
    assert!(matches!(
        err,
        ScanError::Manifest(ManifestLoadError::PackageConformanceFailed { .. })
    ));
}
