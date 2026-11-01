//! Stage E `E7` (`W81-E7A`, `[ROAD-E-001]`) Integration & `H7` Adversarial
//! Conformance Gate: Host `N-1 -> N` Zero-Recompile Cross-Version Upgrade.

#![forbid(unsafe_code)]

use std::fs;
use std::path::Path;

use nlos_artifact::{check_package_file, decode_package_file};
use nlos_certify::json::parse_json;
use nlos_certify::{StoreCertificationReceipt, verify_store_certification_receipt};
use nlos_schema::MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS;
use nlos_slice_k::SliceKRuntime;
use stage_e_road_e001_upgrade_gate::step_a_frozen_seal::run_step_a_verify_frozen_n1_baseline;
use stage_e_road_e001_upgrade_gate::step_b_n1_host::materialize_and_verify_frozen_package;
use stage_e_road_e001_upgrade_gate::step_c_in_place_upgrade::{
    DEPRECATED_N1_REPLACEMENT, DEPRECATED_N1_SYMBOL, build_host_n_evolved_deprecation_manifest,
    verify_compat_ver_002_matrix_on_host_n, verify_kabi_internal_refactor_insulation,
};
use stage_e_road_e001_upgrade_gate::{
    FROZEN_N1_SAMPLES, HOST_EPOCH_N, HOST_EPOCH_N_MINUS_1, SealedBaselineSnapshot,
    TempHostWorkspace, repo_root, run_full_road_e001_upgrade_chain, sha256_hex,
};

#[test]
fn step_a_frozen_n1_baseline_seals_certifications_and_cas_promotions_verify() {
    let root = repo_root();
    let pre = SealedBaselineSnapshot::capture(&root).expect("capture pre-Step-A snapshot");
    assert_eq!(
        pre.files.len(),
        16,
        "sealed N-1 baseline must contain 16 files"
    );

    let step_a = run_step_a_verify_frozen_n1_baseline(&root, &pre)
        .expect("Step A frozen N-1 seal verification must pass");
    assert_eq!(step_a.samples.len(), 4);
    assert_eq!(step_a.verified_nlos_pkg_count, 8);
    assert_eq!(step_a.verified_certify_report_count, 4);
    assert_eq!(step_a.verified_store_seal_count, 4);
    assert_eq!(step_a.promoted_stable_release_count, 4);

    for (sample, expected) in step_a.samples.iter().zip(FROZEN_N1_SAMPLES.iter()) {
        assert_eq!(sample.spec.label, expected.label);
        assert_eq!(
            sha256_hex(&sample.raw_v1_0_0_bytes),
            expected.expected_v1_0_0_sha256
        );
        assert_eq!(
            sample.raw_v1_0_0_bytes.len(),
            expected.expected_v1_0_0_bytes
        );
        assert_eq!(
            sha256_hex(&sample.raw_v1_1_0_bytes),
            expected.expected_v1_1_0_sha256
        );
        assert_eq!(
            sample.raw_v1_1_0_bytes.len(),
            expected.expected_v1_1_0_bytes
        );
    }

    let post = SealedBaselineSnapshot::capture(&root).expect("capture post-Step-A snapshot");
    pre.verify_zero_drift(&post)
        .expect("Step A must not modify any sealed N-1 file");
}

#[test]
fn step_c_kabi_internal_refactor_insulates_frozen_n1_sabi_wire_responses() {
    verify_kabi_internal_refactor_insulation().expect(
        "[KABI-SVC-001] internal KABI refactor must produce byte-identical SABI wire responses",
    );
}

#[test]
fn step_c_compat_ver_002_rejects_unknown_major_critical_ext_and_tombstones_while_preserving_unknown_fields()
 {
    let manifest = build_host_n_evolved_deprecation_manifest()
        .expect("Host N evolved DeprecationManifest must validate");
    assert_eq!(manifest.current_epoch, HOST_EPOCH_N);
    assert!(manifest.min_compatibility_window_epochs >= MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS);

    let (rejection_count, preserved_exts, preserved_proto_bytes) =
        verify_compat_ver_002_matrix_on_host_n(&manifest)
            .expect("[COMPAT-VER-002] matrix on Host N must pass");
    assert!(
        rejection_count >= 6,
        "must verify >= 6 fail-closed E_ABI_VERSION + DO_NOT_RETRY cases, got {rejection_count}"
    );
    assert_eq!(preserved_exts, vec![7701, 7702]);
    assert!(
        preserved_proto_bytes > 0,
        "unknown raw Protobuf wire fields must be preserved across round-trip"
    );
}

#[test]
fn h7_adversarial_tampered_frozen_package_and_forged_certification_fail_closed() {
    let root = repo_root();
    let pre = SealedBaselineSnapshot::capture(&root).expect("capture pre snapshot");
    let obs_spec = &FROZEN_N1_SAMPLES[0];

    // 1. Copy frozen `.nlos-pkg` into memory (never modifying disk!) and prove
    //    1-byte payload tamper & 1-byte signature tamper both fail closed.
    let raw_pkg =
        fs::read(root.join(obs_spec.pkg_v1_0_0_rel_path)).expect("read frozen observatory v1.0.0");
    assert!(
        check_package_file(&raw_pkg).is_conformant(),
        "clean frozen .nlos-pkg must be conformant"
    );

    let mut payload_tampered = decode_package_file(&raw_pkg).expect("decode clean package");
    payload_tampered.entries[0].payload[0] ^= 0x01;
    let ws = TempHostWorkspace::new("h7-tamper");
    let payload_runtime = SliceKRuntime::open(ws.path().join("authority-payload-tamper"))
        .expect("open payload-tamper runtime");
    let payload_err = materialize_and_verify_frozen_package(
        &payload_runtime,
        &payload_tampered,
        b"tampered-payload",
    )
    .expect_err("1-byte payload tamper must fail closed");
    assert!(
        payload_err.contains("DigestMismatch")
            || payload_err.contains("digest")
            || payload_err.contains("Signature"),
        "unexpected payload tamper error: {payload_err}"
    );

    let mut sig_tampered = decode_package_file(&raw_pkg).expect("decode clean package");
    sig_tampered.signature[63] ^= 0x80;
    let sig_runtime = SliceKRuntime::open(ws.path().join("authority-sig-tamper"))
        .expect("open sig-tamper runtime");
    let sig_err =
        materialize_and_verify_frozen_package(&sig_runtime, &sig_tampered, b"tampered-signature")
            .expect_err("1-byte Ed25519 signature tamper must fail closed");
    assert!(
        sig_err.contains("Signature") || sig_err.contains("signature"),
        "unexpected signature tamper error: {sig_err}"
    );

    // 2. Prove 1-byte tamper of `.certify.json` `StoreCertificationReceipt`
    //    fails `verify_store_certification_receipt` fail-closed.
    let certify_raw =
        fs::read_to_string(root.join(obs_spec.certify_json_rel_path)).expect("read certify.json");
    let certify_json = parse_json(&certify_raw).expect("parse certify.json");
    let receipt_json = certify_json
        .as_object()
        .and_then(|obj| obj.get("receipt"))
        .expect("certify.json receipt object");
    let mut forged_receipt =
        StoreCertificationReceipt::from_json_value(receipt_json).expect("decode certify receipt");
    let certifier_pk = forged_receipt.certifier_public_key;
    let issued_at = forged_receipt.issued_at_ms;
    verify_store_certification_receipt(&forged_receipt, Some(&certifier_pk), issued_at)
        .expect("untampered StoreCertificationReceipt must verify");
    forged_receipt.ed25519_signature[0] ^= 0x01;
    let cert_err =
        verify_store_certification_receipt(&forged_receipt, Some(&certifier_pk), issued_at)
            .expect_err("forged StoreCertificationReceipt signature must fail closed");
    assert!(
        format!("{cert_err}").contains("signature")
            || format!("{cert_err:?}").contains("Signature"),
        "unexpected certification verification error: {cert_err:?}"
    );

    // 3. Prove `SealedBaselineSnapshot::verify_zero_drift` catches a 1-byte drift on an
    //    in-memory mutated snapshot, while live disk files remain 100% unchanged.
    let mut drifted_snapshot = pre.clone();
    if let Some((sha, _)) = drifted_snapshot.files.get_mut(obs_spec.pkg_v1_0_0_rel_path) {
        *sha = "0000000000000000000000000000000000000000000000000000000000000000".to_owned();
    }
    let drift_err = pre
        .verify_zero_drift(&drifted_snapshot)
        .expect_err("mutated SHA-256 must trigger zero-recompile violation");
    assert!(drift_err.contains("zero-recompile violation"));

    let post = SealedBaselineSnapshot::capture(&root).expect("capture post snapshot");
    pre.verify_zero_drift(&post)
        .expect("on-disk sealed N-1 baseline must remain completely untouched");
}

#[test]
fn h7_ecosystem_manifests_remain_free_of_private_kernel_crate_dependencies() {
    let root = repo_root();
    let forbidden_kernel_prefixes = [
        "nlos-kernel",
        "nlos-kabi",
        "nlos-hal",
        "nlos-mm",
        "nlos-sched",
        "nlos-arch",
        "nlos-boot",
        "nlos-wasm-sandbox",
        "nlos-compat-shim",
        "nlos-store-registry",
        "nlos-publisher-trust",
        "nlos-certify",
    ];
    let driver_extra_forbidden = [
        "nlos-identity",
        "nlos-capability",
        "nlos-task",
        "nlos-plan",
        "nlos-application",
        "nlos-slice-k",
    ];

    for (rel_manifest, is_driver) in [
        ("examples/ecosystem-apps/Cargo.toml", false),
        (
            "examples/ecosystem-apps/knowledge-observatory-app/Cargo.toml",
            false,
        ),
        ("examples/ecosystem-apps/field-survey-app/Cargo.toml", false),
        ("examples/ecosystem-drivers/Cargo.toml", true),
        (
            "examples/ecosystem-drivers/ecosystem-driver-kit/Cargo.toml",
            true,
        ),
        (
            "examples/ecosystem-drivers/mcp-bridge-driver/Cargo.toml",
            true,
        ),
        (
            "examples/ecosystem-drivers/openai-compat-driver/Cargo.toml",
            true,
        ),
    ] {
        let content =
            fs::read_to_string(root.join(rel_manifest)).expect("read ecosystem Cargo.toml");
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                continue;
            }
            for forbidden in &forbidden_kernel_prefixes {
                assert!(
                    !trimmed.contains(forbidden),
                    "[KABI-SVC-001] forbidden private kernel crate {forbidden:?} referenced in {rel_manifest}: {trimmed}"
                );
            }
            if is_driver {
                for forbidden in &driver_extra_forbidden {
                    assert!(
                        !trimmed.contains(forbidden),
                        "[DRV-PROTO-001] userland driver manifest {rel_manifest} must not link internal host authority crate {forbidden:?}: {trimmed}"
                    );
                }
            }
        }
    }
}

#[test]
fn full_road_e001_n1_to_n_zero_recompile_cross_version_upgrade_chain() {
    let wasm_host_exe = Path::new(env!("CARGO_BIN_EXE_road-e001-wasm-sandbox-host"));
    let receipt = run_full_road_e001_upgrade_chain(wasm_host_exe)
        .expect("[ROAD-E-001] full Step A -> Step B -> Step C -> Step D zero-recompile upgrade chain must pass");

    // Pre == Post zero-recompile / zero-re-sign proof across all 16 sealed `N-1` files.
    assert_eq!(receipt.pre_snapshot, receipt.post_snapshot);
    assert_eq!(receipt.pre_snapshot.files.len(), 16);

    // Step A assertions.
    assert_eq!(receipt.step_a.verified_nlos_pkg_count, 8);
    assert_eq!(receipt.step_a.verified_certify_report_count, 4);
    assert_eq!(receipt.step_a.verified_store_seal_count, 4);
    assert_eq!(receipt.step_a.promoted_stable_release_count, 4);

    // Step B (`N-1` Host, `epoch = 1`) assertions.
    assert_eq!(receipt.step_b.host_epoch, HOST_EPOCH_N_MINUS_1);
    assert_eq!(receipt.step_b.installed_samples.len(), 4);
    assert_eq!(receipt.step_b.observatory_remaining_calls, 7);
    assert_eq!(receipt.step_b.survey_delegated_remaining_calls, 3);
    assert_eq!(receipt.step_b.pinned_dependency_handles.len(), 4);
    assert_eq!(receipt.step_b.mcp_calls_executed_on_n1, 2);
    assert_eq!(receipt.step_b.llm_budget_after_n1.consumed_api_calls, 2);
    assert_eq!(
        receipt.step_b.streamed_sse_completion_text,
        "N-1 baseline sealed"
    );

    // Step C (In-Place Host `N` Upgrade, `epoch = 2`) assertions.
    assert_eq!(receipt.step_c.host_epoch, HOST_EPOCH_N);
    assert!(receipt.step_c.backward_window_epochs >= MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS);
    assert!(receipt.step_c.kabi_refactor_wire_identical);
    assert_eq!(receipt.step_c.native_epoch_n_shim_hops, 0);
    assert_eq!(receipt.step_c.mounted_n1_shim_hops.len(), 4);
    for hops in receipt.step_c.mounted_n1_shim_hops.values() {
        assert!(*hops >= 1, "N-1 package must mount >= 1 compat shim hop");
    }
    assert!(receipt.step_c.fail_closed_rejection_count >= 6);
    assert_eq!(
        receipt.step_c.preserved_non_critical_extensions,
        vec![7701, 7702]
    );
    assert!(receipt.step_c.preserved_unknown_protobuf_bytes > 0);

    // Step D (Host `N` Recovery, Shim Mediation, Driver Reconcile, Sandbox, v1.1.0 Upgrade, Clean Uninstall) assertions.
    let d = &receipt.step_d;
    assert_eq!(d.recovered_installations_count, 4);
    assert!(d.anti_hijack_rejected);
    assert_eq!(d.observatory_remaining_calls_after_host_n_consume, 6);
    assert!(d.llm_budget_recovered_and_idempotent);
    assert_eq!(d.recovered_completed_plan_nodes, 6);
    assert_eq!(d.recovered_user_data_digests_matched, 4);

    assert_eq!(d.deprecation_warnings_emitted.len(), 1);
    assert_eq!(
        d.deprecation_warnings_emitted[0].symbol,
        DEPRECATED_N1_SYMBOL
    );
    assert_eq!(
        d.deprecation_warnings_emitted[0].replacement,
        DEPRECATED_N1_REPLACEMENT
    );
    assert_eq!(d.deprecation_warning_ring_ceiling_verified, 256);
    assert_eq!(d.zero_recompile_payload_runs_on_host_n, 2);

    assert_eq!(d.shim_mediated_secret_leak_bytes, 0);
    assert_eq!(
        d.rotated_driver_generations.get("mcp-bridge-driver"),
        Some(&2)
    );
    assert_eq!(
        d.rotated_driver_generations.get("openai-compat-driver"),
        Some(&2)
    );
    assert!(d.tcb_uncertain_reconciled_with_same_key);
    assert!(d.mcp_uncertain_reconciled_with_same_key);
    assert_eq!(d.wasm_sandbox_isolated_env_vars, 0);

    assert!(d.permission_escalation_blocked_without_z4_reconsent);
    assert!(d.injected_fault_rollback_preserved_gen1_and_user_data);
    assert_eq!(
        d.upgraded_v1_1_0_packages.get("knowledge-observatory-app"),
        Some(&2)
    );
    assert_eq!(
        d.upgraded_v1_1_0_packages.get("mcp-bridge-driver"),
        Some(&2)
    );
    assert_eq!(
        d.upgraded_v1_1_0_packages.get("openai-compat-driver"),
        Some(&2)
    );

    assert!(d.uninstall_blocked_while_bg_task_active);
    assert!(d.revoked_root_and_delegated_capabilities_blocked);
    assert_eq!(d.uninstalled_app_private_cache_remaining_files, 0);
    assert!(d.shared_corpus_artifact_preserved_after_uninstall);
    assert!(d.user_data_preserved_by_default_then_purged_with_z4);
}
