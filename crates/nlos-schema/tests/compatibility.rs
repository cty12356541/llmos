use nlos_schema::sabi::v1::{Envelope, SchemaIdentity};
use nlos_schema::{
    CompatibilityError, MAX_ENVELOPE_BYTES, SABI_ENVELOPE_SCHEMA, decode_sabi_envelope,
    encode_sabi_envelope, schema_registry,
};

fn envelope() -> Envelope {
    Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: 1,
            minor: 0,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: vec![42],
        }),
        request_id: (0_u8..16).collect(),
        service: "operation".to_owned(),
        method: "get".to_owned(),
        payload: b"abc".to_vec(),
    }
}

fn decode_hex(input: &str) -> Vec<u8> {
    let compact = input.trim();
    assert_eq!(compact.len() % 2, 0);
    (0..compact.len())
        .step_by(2)
        .map(|offset| u8::from_str_radix(&compact[offset..offset + 2], 16).unwrap())
        .collect()
}

#[test]
fn generated_encoding_matches_canonical_golden_vector() {
    let expected = decode_hex(include_str!(
        "../../../schema/golden/nlos.sabi.Envelope-v1.hex"
    ));
    let encoded = encode_sabi_envelope(&envelope()).unwrap();

    assert_eq!(encoded, expected);
    assert_eq!(
        decode_sabi_envelope(&expected).unwrap().envelope(),
        &envelope()
    );
}

#[test]
fn registry_exposes_the_supported_contract() {
    let descriptor = schema_registry().first().unwrap();
    assert_eq!(descriptor.name, SABI_ENVELOPE_SCHEMA);
    assert_eq!((descriptor.major, descriptor.minor), (1, 0));
}

#[test]
fn newer_minor_and_noncritical_extensions_are_accepted() {
    let mut candidate = envelope();
    let schema = candidate.schema.as_mut().unwrap();
    schema.minor = 99;
    schema.non_critical_extension_ids.push(7_001);

    let encoded = encode_sabi_envelope(&candidate).unwrap();
    assert_eq!(
        decode_sabi_envelope(&encoded).unwrap().envelope(),
        &candidate
    );
}

#[test]
fn unknown_major_is_rejected() {
    let mut candidate = envelope();
    candidate.schema.as_mut().unwrap().major = 2;

    assert!(matches!(
        encode_sabi_envelope(&candidate),
        Err(CompatibilityError::UnsupportedMajor {
            got: 2,
            supported: 1,
            ..
        })
    ));
}

#[test]
fn unknown_critical_extension_is_rejected() {
    let mut candidate = envelope();
    candidate
        .schema
        .as_mut()
        .unwrap()
        .critical_extension_ids
        .push(7_001);

    assert!(matches!(
        encode_sabi_envelope(&candidate),
        Err(CompatibilityError::UnsupportedCriticalExtension {
            extension_id: 7_001,
            ..
        })
    ));
}

#[test]
fn forwarding_preserves_unknown_protobuf_fields_byte_for_byte() {
    let mut wire = encode_sabi_envelope(&envelope()).unwrap();
    // Unknown field 100, varint value 7. Prost accepts but does not expose it
    // in the generated type, so forwarding must retain the original frame.
    wire.extend_from_slice(&[0xa0, 0x06, 0x07]);

    let validated = decode_sabi_envelope(&wire).unwrap();
    assert_eq!(validated.wire_bytes(), wire);
    assert_eq!(validated.into_wire_bytes(), wire);
}

#[test]
fn malformed_identity_and_oversized_frames_fail_closed() {
    let mut candidate = envelope();
    candidate.request_id.pop();
    assert!(matches!(
        encode_sabi_envelope(&candidate),
        Err(CompatibilityError::InvalidRequestIdLength { actual: 15 })
    ));

    let oversized = vec![0_u8; MAX_ENVELOPE_BYTES + 1];
    assert!(matches!(
        decode_sabi_envelope(&oversized),
        Err(CompatibilityError::FrameTooLarge { .. })
    ));
}
