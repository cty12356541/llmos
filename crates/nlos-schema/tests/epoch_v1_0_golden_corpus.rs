//! Cross-lane W77-INT integration test pinning `schema/golden/epochs/v1.0/`
//! against `nlos-schema` (9/9 frozen registry, `VersionCompatibilityPolicy`,
//! `canonical_deprecation_manifest`, and byte-for-byte deterministic wire
//! round-trip across all 11 epoch v1.0 golden vectors).

use std::fs;
use std::path::{Path, PathBuf};

use nlos_schema::{
    SABI_CHANNEL_CONTROL_SCHEMA, SABI_DRIVER_PROVIDER_SCHEMA, SABI_ENVELOPE_SCHEMA,
    SABI_OPERATION_CONTROL_SCHEMA, SABI_PRINCIPAL_HANDSHAKE_SCHEMA, SABI_SERVICE_DIRECTORY_SCHEMA,
    SABI_SYSTEM_CONTROL_SCHEMA, SABI_TAKEOVER_CONTROL_SCHEMA, SABI_WAIT_CONTROL_SCHEMA,
    VersionCompatibilityPolicy, VersionRoute, canonical_deprecation_manifest,
    decode_create_channel_request, decode_driver_provider_request,
    decode_get_system_control_request, decode_inspect_wait_request,
    decode_principal_handshake_attestation, decode_principal_handshake_challenge,
    decode_query_operation_request, decode_resolve_service_request, decode_sabi_envelope,
    decode_submit_barrier_observation_request, encode_create_channel_request,
    encode_driver_provider_request, encode_get_system_control_request, encode_inspect_wait_request,
    encode_principal_handshake_attestation, encode_principal_handshake_challenge,
    encode_query_operation_request, encode_resolve_service_request, encode_sabi_envelope,
    encode_submit_barrier_observation_request, registry_frozen,
};

fn epoch_v1_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schema/golden/epochs/v1.0")
}

fn decode_hex_file(path: &Path) -> Vec<u8> {
    let raw = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    assert!(raw.ends_with('\n'), "{} must end with LF", path.display());
    let trimmed = raw.trim_end_matches('\n');
    assert_eq!(trimmed.len() % 2, 0, "{} odd hex length", path.display());
    (0..trimmed.len())
        .step_by(2)
        .map(|idx| {
            u8::from_str_radix(&trimmed[idx..idx + 2], 16)
                .unwrap_or_else(|err| panic!("invalid hex in {}: {err}", path.display()))
        })
        .collect()
}

#[test]
fn epoch_v1_0_corpus_envelope_and_core_control_channels_round_trip() {
    let dir = epoch_v1_dir();
    let policy = VersionCompatibilityPolicy::default();
    let manifest = canonical_deprecation_manifest().expect("canonical deprecation manifest");
    assert_eq!(manifest.channels.len(), 9);

    let all_channels = [
        SABI_ENVELOPE_SCHEMA,
        SABI_SERVICE_DIRECTORY_SCHEMA,
        SABI_OPERATION_CONTROL_SCHEMA,
        SABI_SYSTEM_CONTROL_SCHEMA,
        SABI_TAKEOVER_CONTROL_SCHEMA,
        SABI_WAIT_CONTROL_SCHEMA,
        SABI_PRINCIPAL_HANDSHAKE_SCHEMA,
        SABI_DRIVER_PROVIDER_SCHEMA,
        SABI_CHANNEL_CONTROL_SCHEMA,
    ];
    for channel in all_channels {
        assert_eq!(
            registry_frozen(channel),
            Some(true),
            "{channel} must be frozen"
        );
    }

    // 1. Envelope (canonical v1.0)
    let env_wire = decode_hex_file(&dir.join("nlos.sabi.Envelope-v1.0.hex"));
    let env_frame = decode_sabi_envelope(&env_wire).expect("decode Envelope-v1.0");
    let env_outcome = policy
        .adjudicate_schema_identity(env_frame.envelope().schema.as_ref().unwrap())
        .expect("adjudicate Envelope-v1.0");
    assert_eq!(env_outcome.route, VersionRoute::NativeCurrentEpoch);
    assert_eq!(
        encode_sabi_envelope(env_frame.envelope()).expect("re-encode Envelope-v1.0"),
        env_wire
    );

    // 2. Envelope with unknown non_critical_extension_ids + trailing unknown wire tag a00607
    let env_unk_wire =
        decode_hex_file(&dir.join("nlos.sabi.Envelope-unknown-noncritical-v1.0.hex"));
    let (env_unk_frame, env_unk_outcome) = policy
        .adjudicate_envelope_wire(&env_unk_wire)
        .expect("adjudicate Envelope-unknown-noncritical-v1.0");
    assert_eq!(
        env_unk_outcome.retained_non_critical_extensions,
        vec![42, 7001, 9999]
    );
    assert_eq!(env_unk_frame.wire_bytes(), env_unk_wire.as_slice());

    // 3. ServiceDirectory
    let service_dir_wire =
        decode_hex_file(&dir.join("nlos.sabi.ServiceDirectory.ResolveServiceRequest-v1.0.hex"));
    let service_dir_msg =
        decode_resolve_service_request(&service_dir_wire).expect("decode ResolveServiceRequest");
    assert_eq!(
        policy
            .adjudicate_schema_identity(service_dir_msg.schema.as_ref().unwrap())
            .unwrap()
            .route,
        VersionRoute::NativeCurrentEpoch
    );
    assert_eq!(
        encode_resolve_service_request(&service_dir_msg).unwrap(),
        service_dir_wire
    );

    // 4. OperationControl
    let operation_wire =
        decode_hex_file(&dir.join("nlos.sabi.OperationControl.QueryOperationRequest-v1.0.hex"));
    let operation_msg =
        decode_query_operation_request(&operation_wire).expect("decode QueryOperationRequest");
    assert_eq!(
        policy
            .adjudicate_schema_identity(operation_msg.schema.as_ref().unwrap())
            .unwrap()
            .route,
        VersionRoute::NativeCurrentEpoch
    );
    assert_eq!(
        encode_query_operation_request(&operation_msg).unwrap(),
        operation_wire
    );

    // 5. SystemControl (v1.7)
    let system_ctrl_wire =
        decode_hex_file(&dir.join("nlos.sabi.SystemControl.GetSystemControlRequest-v1.0.hex"));
    let system_ctrl_msg = decode_get_system_control_request(&system_ctrl_wire)
        .expect("decode GetSystemControlRequest");
    assert_eq!(
        policy
            .adjudicate_schema_identity(system_ctrl_msg.schema.as_ref().unwrap())
            .unwrap()
            .route,
        VersionRoute::NativeCurrentEpoch
    );
    assert_eq!(
        encode_get_system_control_request(&system_ctrl_msg).unwrap(),
        system_ctrl_wire
    );
}

#[test]
fn epoch_v1_0_corpus_takeover_wait_handshake_driver_and_channel_round_trip() {
    let dir = epoch_v1_dir();
    let policy = VersionCompatibilityPolicy::default();

    // 6. TakeoverControl
    let takeover_wire = decode_hex_file(
        &dir.join("nlos.sabi.TakeoverControl.SubmitBarrierObservationRequest-v1.0.hex"),
    );
    let takeover_msg = decode_submit_barrier_observation_request(&takeover_wire)
        .expect("decode SubmitBarrierObservationRequest");
    assert_eq!(
        policy
            .adjudicate_schema_identity(takeover_msg.schema.as_ref().unwrap())
            .unwrap()
            .route,
        VersionRoute::NativeCurrentEpoch
    );
    assert_eq!(
        encode_submit_barrier_observation_request(&takeover_msg).unwrap(),
        takeover_wire
    );

    // 7. WaitControl
    let wait_wire = decode_hex_file(&dir.join("nlos.sabi.WaitControl.InspectWaitRequest-v1.0.hex"));
    let wait_msg = decode_inspect_wait_request(&wait_wire).expect("decode InspectWaitRequest");
    assert_eq!(
        policy
            .adjudicate_schema_identity(wait_msg.schema.as_ref().unwrap())
            .unwrap()
            .route,
        VersionRoute::NativeCurrentEpoch
    );
    assert_eq!(encode_inspect_wait_request(&wait_msg).unwrap(), wait_wire);

    // 8 & 9. PrincipalHandshake (Challenge + Attestation)
    let handshake_challenge_wire =
        decode_hex_file(&dir.join("nlos.sabi.PrincipalHandshake.Challenge-v1.0.hex"));
    let handshake_challenge = decode_principal_handshake_challenge(&handshake_challenge_wire)
        .expect("decode PrincipalHandshakeChallenge");
    assert_eq!(
        policy
            .adjudicate_schema_identity(handshake_challenge.schema.as_ref().unwrap())
            .unwrap()
            .route,
        VersionRoute::NativeCurrentEpoch
    );
    assert_eq!(
        encode_principal_handshake_challenge(&handshake_challenge).unwrap(),
        handshake_challenge_wire
    );

    let handshake_attestation_wire =
        decode_hex_file(&dir.join("nlos.sabi.PrincipalHandshake.Attestation-v1.0.hex"));
    let handshake_attestation = decode_principal_handshake_attestation(&handshake_attestation_wire)
        .expect("decode PrincipalHandshakeAttestation");
    assert_eq!(
        policy
            .adjudicate_schema_identity(handshake_attestation.schema.as_ref().unwrap())
            .unwrap()
            .route,
        VersionRoute::NativeCurrentEpoch
    );
    assert_eq!(
        encode_principal_handshake_attestation(&handshake_attestation).unwrap(),
        handshake_attestation_wire
    );

    // 10. DriverProvider
    let driver_wire = decode_hex_file(&dir.join("nlos.sabi.DriverProvider.Request-v1.0.hex"));
    let driver_msg =
        decode_driver_provider_request(&driver_wire).expect("decode DriverProviderRequest");
    assert_eq!(
        policy
            .adjudicate_schema_identity(driver_msg.schema.as_ref().unwrap())
            .unwrap()
            .route,
        VersionRoute::NativeCurrentEpoch
    );
    assert_eq!(
        encode_driver_provider_request(&driver_msg).unwrap(),
        driver_wire
    );

    // 11. ChannelControl
    let channel_wire =
        decode_hex_file(&dir.join("nlos.sabi.ChannelControl.CreateChannelRequestWire-v1.0.hex"));
    let channel_msg =
        decode_create_channel_request(&channel_wire).expect("decode CreateChannelRequestWire");
    assert_eq!(
        policy
            .adjudicate_schema_identity(channel_msg.schema.as_ref().unwrap())
            .unwrap()
            .route,
        VersionRoute::NativeCurrentEpoch
    );
    assert_eq!(
        encode_create_channel_request(&channel_msg).unwrap(),
        channel_wire
    );
}
