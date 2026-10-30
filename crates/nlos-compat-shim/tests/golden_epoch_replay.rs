//! Cross-epoch golden corpus replay (`N-1` / `N-2` <-> `N`), unknown-field
//! lossless pass-through (`a00607`), `UNCERTAIN` preservation, sunset rejection,
//! and bounded `DeprecationWarningReceipt` integration tests (`W78-E2`).

use std::fs;
use std::path::{Path, PathBuf};

use nlos_compat_shim::{
    CompatMountRejection, CompatShimMountDecision, CompatShimMountPolicy, MAX_DEPRECATION_WARNINGS,
    PackageCompatibilitySpec, SabiAdapterError, SabiChannelId, SabiEpochAdapterCell, SabiFrameKind,
    evaluate_compat_shim_mount, parse_package_compatibility_manifest,
};
use nlos_schema::sabi::v1::{
    DriverProviderQueryIsolationRequest, DriverProviderRequest, Envelope, OperationReference,
    RetryDirective, SabiErrorCode, SabiFailure, SabiResponseContext, SchemaIdentity,
    driver_provider_request, envelope,
};
use nlos_schema::{SABI_DRIVER_PROVIDER_SCHEMA, SABI_ENVELOPE_SCHEMA, parse_deprecation_manifest};
use prost::Message;
use sha2::{Digest, Sha256};

struct GoldenEntry {
    file: &'static str,
    channel: &'static str,
    kind: SabiFrameKind,
    expected_len: usize,
    expected_sha256: &'static str,
}

const EPOCH_V1_0_GOLDENS: [GoldenEntry; 11] = [
    GoldenEntry {
        file: "nlos.sabi.Envelope-v1.0.hex",
        channel: "nlos.sabi.Envelope",
        kind: SabiFrameKind::Envelope,
        expected_len: 280,
        expected_sha256: "1fcef924ca415e1d44c825755e15ccc9eaaff8480f93d9eb3146fd8cae0c7724",
    },
    GoldenEntry {
        file: "nlos.sabi.Envelope-unknown-noncritical-v1.0.hex",
        channel: "nlos.sabi.Envelope",
        kind: SabiFrameKind::Envelope,
        expected_len: 83,
        expected_sha256: "61508a0f778c4c34fcba6b8b63390381937515718188cd5dbfa40ae03b5e3382",
    },
    GoldenEntry {
        file: "nlos.sabi.ServiceDirectory.ResolveServiceRequest-v1.0.hex",
        channel: "nlos.sabi.ServiceDirectory",
        kind: SabiFrameKind::ResolveServiceRequest,
        expected_len: 48,
        expected_sha256: "f8387d10fc8a72a8f72fcf3f6b0af21358785e505f99d9b19ca2b7aacc58ec70",
    },
    GoldenEntry {
        file: "nlos.sabi.OperationControl.QueryOperationRequest-v1.0.hex",
        channel: "nlos.sabi.OperationControl",
        kind: SabiFrameKind::QueryOperationRequest,
        expected_len: 54,
        expected_sha256: "866a3b0811a681f06992b21ff4bfff290966289fc348bbd7ff25148037d378a4",
    },
    GoldenEntry {
        file: "nlos.sabi.SystemControl.GetSystemControlRequest-v1.0.hex",
        channel: "nlos.sabi.SystemControl",
        kind: SabiFrameKind::GetSystemControlRequest,
        expected_len: 73,
        expected_sha256: "fb1b2a923e60a6fa22444c853cd516eb8bad11ddd51c73b9256833794f9ca0ac",
    },
    GoldenEntry {
        file: "nlos.sabi.TakeoverControl.SubmitBarrierObservationRequest-v1.0.hex",
        channel: "nlos.sabi.TakeoverControl",
        kind: SabiFrameKind::SubmitBarrierObservationRequest,
        expected_len: 274,
        expected_sha256: "988fa2aeab19e3d759a9bd27956da8772a8766a557bb8349efc28975489be892",
    },
    GoldenEntry {
        file: "nlos.sabi.WaitControl.InspectWaitRequest-v1.0.hex",
        channel: "nlos.sabi.WaitControl",
        kind: SabiFrameKind::InspectWaitRequest,
        expected_len: 45,
        expected_sha256: "a90443d446e5e32c284babdae53428f3e078635d2d427bc10aa6611377b8ff10",
    },
    GoldenEntry {
        file: "nlos.sabi.PrincipalHandshake.Challenge-v1.0.hex",
        channel: "nlos.sabi.PrincipalHandshake",
        kind: SabiFrameKind::PrincipalHandshakeChallenge,
        expected_len: 68,
        expected_sha256: "9bae5953652b07d507e1569222ae2b91a7d3a24aa7d120a0975234861f4bce0a",
    },
    GoldenEntry {
        file: "nlos.sabi.PrincipalHandshake.Attestation-v1.0.hex",
        channel: "nlos.sabi.PrincipalHandshake",
        kind: SabiFrameKind::PrincipalHandshakeAttestation,
        expected_len: 193,
        expected_sha256: "06e4bc1042401e2dea8b554ec9172c1ba1a7e9c68d82e20090026c2df9111569",
    },
    GoldenEntry {
        file: "nlos.sabi.DriverProvider.Request-v1.0.hex",
        channel: "nlos.sabi.DriverProvider",
        kind: SabiFrameKind::DriverProviderRequest,
        expected_len: 92,
        expected_sha256: "ab8bebab35db75c4f14be3e8f6ba9dc8ca59f300457100b00a315c5ad81395ac",
    },
    GoldenEntry {
        file: "nlos.sabi.ChannelControl.CreateChannelRequestWire-v1.0.hex",
        channel: "nlos.sabi.ChannelControl",
        kind: SabiFrameKind::CreateChannelRequestWire,
        expected_len: 93,
        expected_sha256: "8723f39ba6eec4552a9511774f4b83305bcaa3f2b686d55d459aa138ee81303d",
    },
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn decode_hex_file(path: &Path) -> Vec<u8> {
    let raw = fs::read_to_string(path).expect("read golden hex file");
    let clean: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    assert_eq!(clean.len() % 2, 0, "hex string must have even length");
    (0..clean.len())
        .step_by(2)
        .map(|idx| u8::from_str_radix(&clean[idx..idx + 2], 16).expect("valid hex byte"))
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    let mut out = String::with_capacity(64);
    for b in digest {
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

#[test]
fn all_11_epoch_v1_0_goldens_replay_across_n_minus_1_and_n_minus_2_deterministically() {
    let goldens_dir = repo_root().join("schema/golden/epochs/v1.0");
    let manifest_raw = fs::read_to_string(goldens_dir.join("manifest.json")).expect("manifest");
    for ch in SabiChannelId::ALL {
        assert!(
            manifest_raw.contains(ch.schema_name()),
            "manifest missing frozen channel {}",
            ch.schema_name()
        );
        assert!(
            EPOCH_V1_0_GOLDENS
                .iter()
                .any(|g| g.channel == ch.schema_name()),
            "golden table missing channel {}",
            ch.schema_name()
        );
    }

    let spec = PackageCompatibilitySpec::new(1, 0);
    // Host at epoch 1 => NativeDirect (zero shim mounted)
    let native = evaluate_compat_shim_mount(&spec, &CompatShimMountPolicy::new(1))
        .expect("epoch 1 native mount");
    assert!(matches!(
        native,
        CompatShimMountDecision::NativeDirect { host_epoch: 1, .. }
    ));

    // Host upgraded to epoch 2 (N-1) and epoch 3 (N-2)
    for host_epoch in [2_u32, 3_u32] {
        let CompatShimMountDecision::MountOutOfCore(mounted) =
            evaluate_compat_shim_mount(&spec, &CompatShimMountPolicy::new(host_epoch))
                .expect("mount out-of-core shim")
        else {
            panic!("expected MountOutOfCore for host_epoch={host_epoch}");
        };
        let mut adapter = mounted.sabi_adapter.expect("mounted SabiEpochAdapterCell");
        assert_eq!(adapter.client_epoch(), 1);
        assert_eq!(adapter.host_epoch(), host_epoch);

        for vector in &EPOCH_V1_0_GOLDENS {
            let original = decode_hex_file(&goldens_dir.join(vector.file));
            assert_eq!(original.len(), vector.expected_len, "{}", vector.file);
            assert_eq!(
                sha256_hex(&original),
                vector.expected_sha256,
                "{}",
                vector.file
            );

            let up1 = adapter
                .translate_upstream_frame(vector.kind, &original)
                .expect("upstream translation");
            let up2 = adapter
                .translate_upstream_frame(vector.kind, &original)
                .expect("deterministic repeat upstream");
            assert_eq!(
                up1.wire_bytes, up2.wire_bytes,
                "byte-deterministic upstream"
            );
            assert_eq!(up1.trace.channel, vector.channel);
            assert_eq!(up1.trace.source_major, 1);
            assert_eq!(up1.trace.target_major, host_epoch);

            let down = adapter
                .translate_downstream_frame(vector.kind, &up1.wire_bytes)
                .expect("downstream translation");
            assert_eq!(down.trace.source_major, host_epoch);
            assert_eq!(down.trace.target_major, 1);
            assert_eq!(
                down.wire_bytes, original,
                "round-trip byte mismatch for {} at host_epoch={host_epoch}",
                vector.file
            );
            assert_eq!(sha256_hex(&down.wire_bytes), vector.expected_sha256);
        }
    }
}

#[test]
fn unknown_non_critical_extensions_and_trailing_protobuf_tag_a00607_survive_losslessly() {
    let path = repo_root()
        .join("schema/golden/epochs/v1.0/nlos.sabi.Envelope-unknown-noncritical-v1.0.hex");
    let wire = decode_hex_file(&path);
    assert!(
        wire.ends_with(&[0xa0, 0x06, 0x07]),
        "expected trailing tag 100 varint 7"
    );

    let mut adapter = SabiEpochAdapterCell::new(1, 3, Vec::new()).expect("N-2 adapter");
    let upstream = adapter
        .translate_upstream_envelope(&wire)
        .expect("upstream N-2");
    assert!(
        upstream.wire_bytes.ends_with(&[0xa0, 0x06, 0x07]),
        "prost decode/re-encode would drop trailing unknown tag a00607; surgical rewriter must keep it"
    );
    assert_eq!(
        upstream.trace.retained_non_critical_extensions,
        vec![42, 7001, 9999]
    );
    assert_eq!(upstream.trace.preserved_unknown_field_numbers, vec![100]);
    assert_eq!(upstream.trace.preserved_unknown_field_bytes, 3);

    let decoded_host_env = Envelope::decode(upstream.wire_bytes.as_slice()).expect("decode host");
    assert_eq!(decoded_host_env.schema.expect("schema").major, 3);

    let downstream = adapter
        .translate_downstream_envelope(&upstream.wire_bytes)
        .expect("downstream N-2");
    assert_eq!(downstream.wire_bytes, wire);
}

#[test]
fn request_and_uncertain_response_goldens_preserve_idempotency_auth_and_reconciliation() {
    let root = repo_root().join("schema/golden");
    let req_wire = decode_hex_file(&root.join("nlos.sabi.Envelope-common-request-v1.hex"));
    let unc_wire = decode_hex_file(&root.join("nlos.sabi.Envelope-common-uncertain-v1.hex"));

    let mut adapter = SabiEpochAdapterCell::new(1, 2, Vec::new()).expect("N-1 adapter");
    let up_req = adapter
        .translate_upstream_envelope(&req_wire)
        .expect("upstream common request");
    assert!(up_req.trace.preserved_request_id.is_some());
    assert!(up_req.trace.preserved_idempotency_key.is_some());
    assert!(up_req.trace.preserved_deadline_monotonic_ns.is_some());
    assert!(up_req.trace.preserved_cancel_epoch.is_some());
    assert!(up_req.trace.preserved_authorization_digest.is_some());

    // Simulate host N=2 returning the canonical UNCERTAIN response envelope
    let host_unc = adapter
        .translate_upstream_envelope(&unc_wire)
        .expect("stamp uncertain golden to host_epoch=2")
        .wire_bytes;
    let down_unc = adapter
        .translate_downstream_envelope(&host_unc)
        .expect("downstream uncertain response");
    assert!(down_unc.trace.preserved_uncertain_outcome);
    assert_eq!(down_unc.wire_bytes, unc_wire);

    // Stripping OperationReference from an UNCERTAIN response MUST fail closed
    let bad_unc = Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: 2,
            minor: 1,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        request_id: vec![0x11; 16],
        service: "operation".to_owned(),
        method: "execute".to_owned(),
        common_context: Some(envelope::CommonContext::ResponseContext(
            SabiResponseContext {
                correlation_id: vec![0x22; 16],
                operation: None,
                receipts: Vec::new(),
                failure: Some(SabiFailure {
                    code: SabiErrorCode::Uncertain.into(),
                    retry: RetryDirective::QueryOperationOrRetrySameIdempotencyKey.into(),
                    safe_message: "uncertain without operation ref".to_owned(),
                }),
            },
        )),
        payload: Vec::new(),
    };
    let err = adapter
        .translate_downstream_envelope(&bad_unc.encode_to_vec())
        .expect_err("must reject UNCERTAIN without OperationReference");
    assert!(matches!(err, SabiAdapterError::CommonSemantics(_)));

    // Valid UNCERTAIN with OperationReference succeeds
    let mut valid_unc = bad_unc;
    if let Some(envelope::CommonContext::ResponseContext(ref mut ctx)) = valid_unc.common_context {
        ctx.operation = Some(OperationReference {
            operation_id: vec![0x33; 16],
            generation: 1,
        });
    }
    let ok = adapter
        .translate_downstream_envelope(&valid_unc.encode_to_vec())
        .expect("valid uncertain preserved");
    assert!(ok.trace.preserved_uncertain_outcome);
}

#[test]
fn host_epoch_4_sunset_zero_future_and_unknown_critical_extensions_fail_closed_with_guidance() {
    let manifest_text = "\
        # starter-compat package\n\
        package-id = 4e1077c1a5b24d8e9f0123456789abcd\n\
        version = 1.0.0\n\
        compatibility.target-sabi-epoch = 1\n\
        compatibility.target-sabi-minor = 1\n\
        compatibility.non-critical-extensions = 42,7001\n\
    ";
    let parsed = parse_package_compatibility_manifest(manifest_text).expect("parse manifest");
    assert_eq!(parsed.target_sabi_epoch, 1);
    assert_eq!(parsed.target_sabi_minor, 1);
    assert_eq!(parsed.non_critical_extensions, vec![42, 7001]);
    let roundtrip = parse_package_compatibility_manifest(&parsed.to_manifest_lines())
        .expect("round-trip manifest lines");
    assert_eq!(roundtrip, parsed);

    // 1. Sunset at host_epoch = 4 (4 - 1 = 3 > max_backward_epochs = 2)
    let sunset = evaluate_compat_shim_mount(&parsed, &CompatShimMountPolicy::new(4))
        .expect_err("v1 must sunset on host_epoch=4");
    assert!(matches!(
        sunset,
        CompatMountRejection::SunsetTargetSabiEpoch {
            target_epoch: 1,
            host_epoch: 4,
            max_backward_epochs: 2
        }
    ));
    let failure = sunset.to_sabi_failure();
    assert_eq!(failure.code, i32::from(SabiErrorCode::AbiVersion));
    assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));
    assert!(sunset.migration_guidance().contains("nlos-migrate"));

    // 2. Zero epoch
    let zero = evaluate_compat_shim_mount(
        &PackageCompatibilitySpec::new(0, 0),
        &CompatShimMountPolicy::new(2),
    )
    .expect_err("epoch 0 invalid");
    assert_eq!(zero, CompatMountRejection::ZeroTargetSabiEpoch);
    assert_eq!(
        zero.to_sabi_failure().code,
        i32::from(SabiErrorCode::AbiVersion)
    );
    assert!(zero.migration_guidance().contains("nlos-migrate"));

    // 3. Future epoch (> host_epoch)
    let future = evaluate_compat_shim_mount(
        &PackageCompatibilitySpec::new(5, 0),
        &CompatShimMountPolicy::new(3),
    )
    .expect_err("future epoch invalid");
    assert!(matches!(
        future,
        CompatMountRejection::FutureTargetSabiEpoch {
            target_epoch: 5,
            host_epoch: 3
        }
    ));
    assert!(future.migration_guidance().contains("nlos-migrate"));

    // 4. Unknown critical extension
    let crit = evaluate_compat_shim_mount(
        &PackageCompatibilitySpec::new(2, 0).with_critical_extensions(vec![777]),
        &CompatShimMountPolicy::new(3).with_known_critical_extensions(vec![100]),
    )
    .expect_err("unknown critical extension");
    assert_eq!(
        crit,
        CompatMountRejection::UnsupportedCriticalExtension { extension_id: 777 }
    );
    assert_eq!(
        crit.to_sabi_failure().code,
        i32::from(SabiErrorCode::AbiVersion)
    );
    assert_eq!(
        crit.to_sabi_failure().retry,
        i32::from(RetryDirective::DoNotRetry)
    );
    assert!(crit.migration_guidance().contains("nlos-migrate"));
}

const CUSTOM_DEPRECATION_YAML: &str = r#"manifest_version: 1
current_epoch: 3
min_compatibility_window_epochs: 2
frozen_channels_total: 9
channels:
  - name: "nlos.sabi.Envelope"
    major: 1
    minor: 1
    frozen: true
    reserved_field_numbers: [99]
    reserved_symbols: ["legacy.forbidden_rpc"]
  - name: "nlos.sabi.ServiceDirectory"
    major: 1
    minor: 0
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []
  - name: "nlos.sabi.OperationControl"
    major: 1
    minor: 0
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []
  - name: "nlos.sabi.SystemControl"
    major: 1
    minor: 7
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []
  - name: "nlos.sabi.TakeoverControl"
    major: 1
    minor: 0
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []
  - name: "nlos.sabi.WaitControl"
    major: 1
    minor: 0
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []
  - name: "nlos.sabi.PrincipalHandshake"
    major: 1
    minor: 0
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []
  - name: "nlos.sabi.DriverProvider"
    major: 1
    minor: 0
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []
  - name: "nlos.sabi.ChannelControl"
    major: 1
    minor: 0
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []
entries:
  - channel: "nlos.sabi.Envelope"
    symbol: "operation.legacy_query"
    kind: "METHOD"
    introduced_in_epoch: 1
    state: "DEPRECATED"
    deprecated_in_epoch: 1
    earliest_sunset_epoch: 3
    replacement: "operation.query"
    migration_rule_id: "MIG-ENV-001"
    reason: "Unified under operation.query in v2+"
  - channel: "nlos.sabi.Envelope"
    symbol: "operation.sunset_rpc"
    kind: "METHOD"
    introduced_in_epoch: 1
    state: "SUNSET"
    deprecated_in_epoch: 1
    earliest_sunset_epoch: 3
    sunset_in_epoch: 3
    replacement: "operation.query"
    migration_rule_id: "MIG-ENV-002"
    reason: "Retired at epoch 3"
"#;

#[test]
fn deprecation_manifest_emits_bounded_warning_receipts_and_blocks_sunset_or_reserved_elements() {
    let manifest =
        parse_deprecation_manifest(CUSTOM_DEPRECATION_YAML).expect("valid custom manifest");
    let mut adapter =
        SabiEpochAdapterCell::with_manifest(1, 3, 2, Vec::new(), manifest).expect("adapter");

    let make_env = |service: &str, method: &str| {
        Envelope {
            schema: Some(SchemaIdentity {
                name: SABI_ENVELOPE_SCHEMA.to_owned(),
                major: 1,
                minor: 1,
                critical_extension_ids: Vec::new(),
                non_critical_extension_ids: Vec::new(),
            }),
            request_id: vec![0x42; 16],
            service: service.to_owned(),
            method: method.to_owned(),
            common_context: None,
            payload: b"ok".to_vec(),
        }
        .encode_to_vec()
    };

    // 1. DEPRECATED method forwards cleanly and emits bounded DeprecationWarningReceipt
    let deprecated_wire = make_env("operation", "legacy_query");
    for _ in 0..300 {
        let translated = adapter
            .translate_upstream_envelope(&deprecated_wire)
            .expect("deprecated method must still forward");
        assert_eq!(translated.trace.emitted_deprecation_warnings.len(), 1);
        let warning = &translated.trace.emitted_deprecation_warnings[0];
        assert_eq!(warning.replacement_symbol, "operation.query");
        assert_eq!(warning.migration_rule_id.as_deref(), Some("MIG-ENV-001"));
    }
    assert_eq!(
        adapter.deprecation_warnings().len(),
        MAX_DEPRECATION_WARNINGS
    );

    // 2. SUNSET method fails closed with AbiVersion + DoNotRetry
    let sunset_err = adapter
        .translate_upstream_envelope(&make_env("operation", "sunset_rpc"))
        .expect_err("SUNSET method must be blocked");
    assert_eq!(
        sunset_err.to_sabi_failure().code,
        i32::from(SabiErrorCode::AbiVersion)
    );
    assert_eq!(
        sunset_err.to_sabi_failure().retry,
        i32::from(RetryDirective::DoNotRetry)
    );

    // 3. RESERVED tombstone field number 99 on wire (`(99 << 3) | 0 = 792 => 0x98, 0x06`, val `0x01`)
    let mut reserved_field_wire = make_env("operation", "query");
    reserved_field_wire.extend_from_slice(&[0x98, 0x06, 0x01]);
    let reserved_err = adapter
        .translate_upstream_envelope(&reserved_field_wire)
        .expect_err("RESERVED field 99 must be blocked on raw wire");
    assert!(matches!(
        reserved_err,
        SabiAdapterError::SunsetOrReservedContract { .. }
    ));
}

#[test]
fn nested_envelope_with_typed_channel_payload_translates_both_layers_atomically() {
    let inner_req = DriverProviderRequest {
        schema: Some(SchemaIdentity {
            name: SABI_DRIVER_PROVIDER_SCHEMA.to_owned(),
            major: 1,
            minor: 0,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: vec![88],
        }),
        request: Some(driver_provider_request::Request::QueryIsolation(
            DriverProviderQueryIsolationRequest {},
        )),
    };
    let mut outer_wire = Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: 1,
            minor: 1,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: vec![77],
        }),
        request_id: vec![0x99; 16],
        service: "driver_provider".to_owned(),
        method: "query_isolation".to_owned(),
        common_context: None,
        payload: inner_req.encode_to_vec(),
    }
    .encode_to_vec();
    outer_wire.extend_from_slice(&[0xa0, 0x06, 0x07]);

    let mut adapter = SabiEpochAdapterCell::new(1, 3, Vec::new()).expect("N-2");
    let up = adapter
        .translate_upstream_envelope_with_payload(&outer_wire, SabiFrameKind::DriverProviderRequest)
        .expect("atomic nested upstream");
    assert!(up.wire_bytes.ends_with(&[0xa0, 0x06, 0x07]));

    let host_env = Envelope::decode(up.wire_bytes.as_slice()).expect("host env");
    assert_eq!(host_env.schema.expect("schema").major, 3);
    let host_inner =
        DriverProviderRequest::decode(host_env.payload.as_slice()).expect("host inner");
    assert_eq!(host_inner.schema.expect("inner schema").major, 3);

    let down = adapter
        .translate_downstream_envelope_with_payload(
            &up.wire_bytes,
            SabiFrameKind::DriverProviderRequest,
        )
        .expect("atomic nested downstream");
    assert_eq!(down.wire_bytes, outer_wire);
}
